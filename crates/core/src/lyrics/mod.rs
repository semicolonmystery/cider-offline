//! Lyrics: Apple TTML observed through the proxy is saved under the lyrics
//! directory, other providers fill the gaps (their formats converted to
//! TTML: Cider's community lyrics and its Musixmatch backend, the AMLL TTML
//! DB, Netease, Kugou and LRCLIB), and lyrics
//! requests are answered from the local files when offline or when Apple has
//! none.
//!
//! Files: `<id>.ttml` (line-timed) and `<id>.syllable.ttml` (word-timed),
//! where `<id>` is the catalog id the lyrics endpoints use, plus
//! `<id>.custom.ttml` for lyrics the user imported. Custom lyrics live in a
//! file of their own so no Apple answer or provider lookup can overwrite
//! them, and [`LyricsStore::read`] always returns them first.

mod amll;
mod convert;
mod engine;
mod flags;
mod interceptor;
mod kugou;
mod lrclib;
mod matching;
mod netease;
mod provider;
mod rise;
mod settings;
mod ttml;
mod web;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

pub use crate::apple::LyricsKind;
pub use crate::storage::LyricsQuality;
pub use amll::AmllProvider;
pub(crate) use engine::{LookupMode, LyricsEngine, TrackOutcome};
pub use engine::{LyricsProgress, LyricsReport};
pub use flags::LyricsFlagsInterceptor;
pub use interceptor::LyricsInterceptor;
pub use kugou::KugouProvider;
pub use lrclib::{FoundLyrics, LRCLIB_BASE_URL, LrclibClient, LyricsQuery, USER_AGENT};
pub use netease::NeteaseProvider;
pub use provider::{
    AppleLyricsProvider, FetchedLyrics, LrclibProvider, LyricsProvider, LyricsRequest,
    ProviderError,
};
pub use rise::{CommunityProvider, MusixmatchProvider};
pub use settings::{
    FailurePolicy, LyricsProviderId, LyricsSettings, MAX_RETRY_PASSES, MAX_SPACING_MS,
    ProviderSetting, TARGET_QUALITIES,
};

pub(crate) use ttml::{lrc_to_ttml, plain_to_ttml, ttml_quality};

use crate::artwork::remove_quietly;
use crate::media::is_valid_media_id;

/// Errors fetching or storing lyrics.
#[derive(Debug, thiserror::Error)]
pub enum LyricsError {
    /// HTTP error talking to LRCLIB.
    #[error("lyrics request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// LRCLIB answered with an error status.
    #[error("lyrics server returned HTTP {status}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// The server's `Retry-After`, when it sent one in seconds.
        retry_after: Option<std::time::Duration>,
    },
    /// File system error.
    #[error("lyrics file error: {0}")]
    Io(#[from] std::io::Error),
    /// The id cannot be used as a file name.
    #[error("invalid track id {0:?}")]
    InvalidId(String),
}

/// Local TTML files, one per track and [`LyricsKind`].
///
/// Clones share an in-memory index of which ids have lyrics and how finely
/// they are timed (see [`Self::quality`]), kept current by every write and
/// removal through any clone and filled from disk by [`Self::load_index`].
#[derive(Debug, Clone)]
pub struct LyricsStore {
    dir: PathBuf,
    index: Arc<RwLock<HashMap<String, LyricsQuality>>>,
}

impl LyricsStore {
    /// Store rooted at `dir` (normally [`crate::AppPaths::lyrics_dir`]).
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            index: Arc::default(),
        }
    }

    /// Timing of the lyrics [`Self::read`] would serve for `id`, from the
    /// in-memory index (no disk access); `None` when `id` has no lyrics.
    pub fn quality(&self, id: &str) -> Option<LyricsQuality> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .copied()
    }

    /// Every indexed id with its lyrics timing, sorted by id.
    pub fn indexed(&self) -> Vec<(String, LyricsQuality)> {
        let mut entries: Vec<_> = self
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, quality)| (id.clone(), *quality))
            .collect();
        entries.sort();
        entries
    }

    /// Whether the index knows no lyrics at all (nothing to annotate).
    pub fn index_is_empty(&self) -> bool {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// Re-reads the lyrics of `id` from disk into the index.
    async fn refresh(&self, id: &str) {
        let quality = self
            .read(id, LyricsKind::Syllable)
            .await
            .map(|(ttml, _)| ttml_quality(&ttml));
        let mut index = self.index.write().unwrap_or_else(PoisonError::into_inner);
        match quality {
            Some(quality) => index.insert(id.to_owned(), quality),
            None => index.remove(id),
        };
    }

    /// Fills the index from the files in the store directory; returns how
    /// many ids have lyrics. A missing directory is an empty store.
    pub async fn load_index(&self) -> usize {
        let mut ids = std::collections::BTreeSet::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(id) = entry.file_name().to_str().and_then(id_of_file) {
                    ids.insert(id.to_owned());
                }
            }
        }
        for id in &ids {
            self.refresh(id).await;
        }
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// File for `id` and `kind`; `None` if `id` is not a safe file name.
    pub fn path(&self, id: &str, kind: LyricsKind) -> Option<PathBuf> {
        is_valid_media_id(id).then(|| {
            self.dir.join(match kind {
                LyricsKind::Line => format!("{id}.ttml"),
                LyricsKind::Syllable => format!("{id}.syllable.ttml"),
            })
        })
    }

    /// File of the user's custom lyrics for `id`; `None` if `id` is not a
    /// safe file name.
    pub fn custom_path(&self, id: &str) -> Option<PathBuf> {
        is_valid_media_id(id).then(|| self.dir.join(format!("{id}.custom.ttml")))
    }

    /// The user's custom lyrics for `id`, if any.
    pub async fn read_custom(&self, id: &str) -> Option<String> {
        let path = self.custom_path(id)?;
        let text = tokio::fs::read_to_string(&path).await.ok()?;
        (!text.trim().is_empty()).then_some(text)
    }

    /// Whether `id` has custom lyrics.
    pub async fn has_custom(&self, id: &str) -> bool {
        self.read_custom(id).await.is_some()
    }

    /// Stores the user's custom lyrics for `id` (atomically). They take
    /// precedence over every other file of `id` from now on.
    ///
    /// # Errors
    ///
    /// Returns [`LyricsError::InvalidId`] or I/O errors.
    pub async fn write_custom(&self, id: &str, ttml: &str) -> Result<(), LyricsError> {
        let path = self
            .custom_path(id)
            .ok_or_else(|| LyricsError::InvalidId(id.to_owned()))?;
        write_atomically(&self.dir, &path, ttml).await?;
        self.refresh(id).await;
        Ok(())
    }

    /// Reads the TTML to serve for `id`: the user's custom lyrics when there
    /// are any (reported as [`LyricsKind::Line`]), otherwise `preferred`,
    /// falling back to the other kind. Returns the kind actually found.
    pub async fn read(&self, id: &str, preferred: LyricsKind) -> Option<(String, LyricsKind)> {
        if let Some(custom) = self.read_custom(id).await {
            return Some((custom, LyricsKind::Line));
        }
        self.read_provider(id, preferred).await
    }

    /// Like [`Self::read`], but ignores custom lyrics (only the files Apple
    /// and the lyrics providers write).
    pub async fn read_provider(
        &self,
        id: &str,
        preferred: LyricsKind,
    ) -> Option<(String, LyricsKind)> {
        let other = match preferred {
            LyricsKind::Line => LyricsKind::Syllable,
            LyricsKind::Syllable => LyricsKind::Line,
        };
        for kind in [preferred, other] {
            let path = self.path(id, kind)?;
            if let Ok(text) = tokio::fs::read_to_string(&path).await
                && !text.trim().is_empty()
            {
                return Some((text, kind));
            }
        }
        None
    }

    /// Whether any TTML exists for `id`.
    pub async fn exists(&self, id: &str) -> bool {
        let paths = [
            self.path(id, LyricsKind::Line),
            self.path(id, LyricsKind::Syllable),
            self.custom_path(id),
        ];
        for path in paths.into_iter().flatten() {
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return true;
            }
        }
        false
    }

    /// Writes TTML atomically (temp file + rename).
    ///
    /// # Errors
    ///
    /// Returns [`LyricsError::InvalidId`] or I/O errors.
    pub async fn write(&self, id: &str, kind: LyricsKind, ttml: &str) -> Result<(), LyricsError> {
        let path = self
            .path(id, kind)
            .ok_or_else(|| LyricsError::InvalidId(id.to_owned()))?;
        write_atomically(&self.dir, &path, ttml).await?;
        self.refresh(id).await;
        Ok(())
    }

    /// Deletes every lyrics file of `id`, custom lyrics included (missing
    /// files are fine).
    pub async fn remove(&self, id: &str) {
        let paths = [
            self.path(id, LyricsKind::Line),
            self.path(id, LyricsKind::Syllable),
            self.custom_path(id),
        ];
        for path in paths.into_iter().flatten() {
            remove_quietly(&path).await;
        }
        self.index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    /// Whether `id` has lyrics from Apple or a provider (not custom ones).
    pub async fn has_provider(&self, id: &str) -> bool {
        let paths = [
            self.path(id, LyricsKind::Line),
            self.path(id, LyricsKind::Syllable),
        ];
        for path in paths.into_iter().flatten() {
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return true;
            }
        }
        false
    }

    /// Deletes the lyrics files Apple answers and providers left for `id`
    /// (`<id>.ttml`, `<id>.syllable.ttml`), keeping the user's custom lyrics
    /// (missing files are fine).
    pub async fn remove_provider(&self, id: &str) {
        let paths = [
            self.path(id, LyricsKind::Line),
            self.path(id, LyricsKind::Syllable),
        ];
        for path in paths.into_iter().flatten() {
            remove_quietly(&path).await;
        }
        self.refresh(id).await;
    }

    /// Moves the lyrics files of `from` to `into` (a song that took another
    /// id) without replacing any of `into`'s: the custom file when `into` has
    /// none, the Apple and provider files (both kinds together) when `into`
    /// has neither. Files that stay are left under `from`. Refreshes the
    /// index of both ids; returns whether anything moved.
    pub async fn move_to(&self, from: &str, into: &str) -> bool {
        let mut moved = false;
        if !self.has_custom(into).await
            && let (Some(src), Some(dst)) = (self.custom_path(from), self.custom_path(into))
        {
            moved |= move_without_replacing(&src, &dst).await;
        }
        if !self.has_provider(into).await {
            for kind in [LyricsKind::Line, LyricsKind::Syllable] {
                if let (Some(src), Some(dst)) = (self.path(from, kind), self.path(into, kind)) {
                    moved |= move_without_replacing(&src, &dst).await;
                }
            }
        }
        if moved {
            self.refresh(from).await;
            self.refresh(into).await;
            tracing::debug!(%from, %into, "lyrics moved to the song's new id");
        }
        moved
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The id a lyrics file name belongs to (`<id>.ttml`, `<id>.syllable.ttml`
/// or `<id>.custom.ttml`); `None` for any other file.
fn id_of_file(name: &str) -> Option<&str> {
    let id = name
        .strip_suffix(".custom.ttml")
        .or_else(|| name.strip_suffix(".syllable.ttml"))
        .or_else(|| name.strip_suffix(".ttml"))?;
    is_valid_media_id(id).then_some(id)
}

/// Moves file `from` to `to` unless `to` exists; returns whether it moved.
/// A hard link claims `to` atomically (it fails when `to` exists); where the
/// file system has no hard links, an existence check and a rename do. A
/// missing `from` moves nothing.
async fn move_without_replacing(from: &Path, to: &Path) -> bool {
    match tokio::fs::hard_link(from, to).await {
        Ok(()) => {
            if let Err(error) = tokio::fs::remove_file(from).await {
                tracing::warn!(file = %from.display(), %error, "could not remove moved lyrics");
            }
            true
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
            ) =>
        {
            false
        }
        Err(_) => {
            if tokio::fs::try_exists(to).await.unwrap_or(true)
                || !tokio::fs::try_exists(from).await.unwrap_or(false)
            {
                return false;
            }
            match tokio::fs::rename(from, to).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(file = %from.display(), %error, "could not move lyrics");
                    false
                }
            }
        }
    }
}

/// Writes `text` to `path` through a temp file + rename, creating `dir`.
async fn write_atomically(dir: &Path, path: &Path, text: &str) -> Result<(), LyricsError> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, text).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_should_prefer_custom_lyrics_over_both_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store.write("1", LyricsKind::Line, "line").await.unwrap();
        store
            .write("1", LyricsKind::Syllable, "word")
            .await
            .unwrap();
        store.write_custom("1", "mine").await.unwrap();
        assert_eq!(
            store.read("1", LyricsKind::Syllable).await,
            Some(("mine".to_owned(), LyricsKind::Line))
        );
    }

    #[tokio::test]
    async fn provider_write_should_not_touch_custom_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store.write_custom("1", "mine").await.unwrap();
        store.write("1", LyricsKind::Line, "apple").await.unwrap();
        assert_eq!(store.read_custom("1").await.as_deref(), Some("mine"));
    }

    #[tokio::test]
    async fn write_should_index_timing() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store
            .write("1", LyricsKind::Line, r#"<tt><p begin="1">a</p></tt>"#)
            .await
            .unwrap();
        assert_eq!(store.quality("1"), Some(LyricsQuality::Line));
    }

    #[tokio::test]
    async fn remove_should_drop_index_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store.write("1", LyricsKind::Line, "<tt/>").await.unwrap();
        store.remove("1").await;
        assert_eq!(store.quality("1"), None);
    }

    #[tokio::test]
    async fn load_index_should_find_every_kind_of_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.ttml"), "<tt/>").unwrap();
        std::fs::write(dir.path().join("2.syllable.ttml"), "<tt/>").unwrap();
        std::fs::write(dir.path().join("local-3.custom.ttml"), "<tt/>").unwrap();
        std::fs::write(dir.path().join("4.part"), "<tt/>").unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        assert_eq!(store.load_index().await, 3);
    }

    #[tokio::test]
    async fn clones_should_share_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        let other = store.clone();
        other.write_custom("1", "<tt/>").await.unwrap();
        assert_eq!(store.quality("1"), Some(LyricsQuality::Plain));
    }

    #[tokio::test]
    async fn remove_should_delete_custom_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store.write_custom("1", "mine").await.unwrap();
        store.remove("1").await;
        assert!(!store.exists("1").await);
    }

    #[tokio::test]
    async fn read_should_fall_back_to_other_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store.write("1", LyricsKind::Line, "<tt/>").await.unwrap();
        assert_eq!(
            store.read("1", LyricsKind::Syllable).await,
            Some(("<tt/>".to_owned(), LyricsKind::Line))
        );
    }

    #[tokio::test]
    async fn write_should_reject_unsafe_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        assert!(
            store
                .write("../x", LyricsKind::Line, "<tt/>")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn remove_should_delete_both_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        store.write("1", LyricsKind::Line, "a").await.unwrap();
        store.write("1", LyricsKind::Syllable, "b").await.unwrap();
        store.remove("1").await;
        assert!(!store.exists("1").await);
    }
}
