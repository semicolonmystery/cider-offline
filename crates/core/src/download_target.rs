//! What a queued download actually fetches.
//!
//! A download row names a track by whatever id the user picked: a catalog
//! (adam) id, or a library id (`i.…` / `a.…`). Before calling the downloader
//! the queue resolves it here into a [`Target`]: the catalog id to fetch, the
//! [`PlaybackId`] shape Cider would send for it, and its [`MediaKind`].
//!
//! # Request shape (from Cider's captured traffic)
//!
//! - A music video is always a catalog play: `{"salableAdamId": …}`, license
//!   `isLibrary: false`, even when started from the library.
//! - A song with a library copy that carries `playParams.catalogId` is a
//!   library play: `{"universalLibraryId": <that copy>, "subscriptionAdamId":
//!   <catalog id>}`, license `isLibrary: true`.
//! - Anything else is a catalog play.
//!
//! A library id is only ever used when its stored resource proves the link
//! (`playParams.catalogId`): `webPlayback` with an unlinked `i.…` id returns an
//! empty `songList`.
//!
//! # Library-only items
//!
//! A library song without `playParams` (for example `i.FakeLibSong0001`,
//! "LATE NIGHT") is not linked to the catalog and Cider never plays it. Its
//! playable *twin* is another item with the same title, artist, album, track
//! number and length: first another library copy that has `playParams`
//! (`a.6814096109`), else a catalog search hit (also matching the ISRC when
//! both sides have one). The twin is downloaded instead and, once done, the
//! original row is linked to it ([`Storage::link_catalog_twin`]) so it shows
//! as downloaded. Without a twin the item is "Can't download yet".

use async_trait::async_trait;
use serde_json::Value;

use crate::catalog::{CatalogClient, CatalogError, SongCandidate};
use crate::download::{DownloadError, MediaKind, PlaybackId};
use crate::storage::{Storage, StorageError, Track, TrackRecord};

/// Largest length difference (ms) between an item and its twin.
const DURATION_TOLERANCE_MS: u32 = 1_000;
/// Search hits inspected when looking for a catalog twin.
const SEARCH_LIMIT: u8 = 25;

/// "Can't download yet" reason of a library-only item without a twin.
pub(crate) const NO_TWIN_REASON: &str = "Library-only item (no catalog link) and no playable copy with the same \
     title, artist, album, track number and length was found";
/// "Can't download yet" reason of a music video while the downloader lacks
/// music-video support.
pub(crate) const MUSIC_VIDEO_REASON: &str = "Music video: saving its audio is not supported yet";

/// What one download fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The catalog (adam) id fetched; the file is `<catalog_id>.m4a`.
    pub catalog_id: String,
    /// The request shape to send.
    pub playback: PlaybackId,
    /// Song or music video.
    pub kind: MediaKind,
    /// The library-only item this target stands in for, when a twin was
    /// resolved.
    pub twin_of: Option<String>,
}

/// Catalog search used to find a twin (the real one is [`CatalogClient`]).
#[async_trait]
pub(crate) trait TwinSearch: Send + Sync {
    /// Playable catalog songs matching `term`.
    async fn search_songs(&self, term: &str) -> Result<Vec<SongCandidate>, CatalogError>;
}

#[async_trait]
impl TwinSearch for CatalogClient {
    async fn search_songs(&self, term: &str) -> Result<Vec<SongCandidate>, CatalogError> {
        CatalogClient::search_songs(self, term, SEARCH_LIMIT).await
    }
}

/// Resolves the queued id `track_id` into a [`Target`].
///
/// # Errors
///
/// [`DownloadError::Unavailable`] for a library-only item without a twin;
/// search failures as [`DownloadError::Transient`] /
/// [`DownloadError::AuthExpired`]; store failures as transient.
pub(crate) async fn resolve(
    storage: &Storage,
    search: &dyn TwinSearch,
    track_id: &str,
) -> Result<Target, DownloadError> {
    if is_catalog_id(track_id) {
        return catalog_target(storage, track_id, None, None).await;
    }
    let rows = storage
        .tracks_by_any_id(track_id)
        .await
        .map_err(|error| store_error(&error))?;
    let Some(own) = rows.into_iter().find(|row| row.track.id == track_id) else {
        return Err(DownloadError::unavailable(
            "Unknown library item: open it in Cider once so its details are stored, then retry",
        ));
    };
    if let Some(catalog_id) = own.catalog_id.as_deref().filter(|id| is_catalog_id(id)) {
        return catalog_target(storage, catalog_id, Some(track_id), None).await;
    }
    let twin = match local_twin(storage, &own.track).await? {
        Some(twin) => Some(twin),
        None => remote_twin(search, &own.track).await?,
    };
    let Some(twin) = twin else {
        return Err(DownloadError::unavailable(NO_TWIN_REASON));
    };
    tracing::info!(item = %track_id, twin = %twin, "library-only item: downloading its playable twin");
    catalog_target(storage, &twin, None, Some(track_id)).await
}

/// The target for catalog id `catalog_id`; `prefer` is a library id to use
/// for a library play when it qualifies.
async fn catalog_target(
    storage: &Storage,
    catalog_id: &str,
    prefer: Option<&str>,
    twin_of: Option<&str>,
) -> Result<Target, DownloadError> {
    let rows = storage
        .tracks_by_any_id(catalog_id)
        .await
        .map_err(|error| store_error(&error))?;
    let kind = if rows
        .iter()
        .any(|row| row.track.metadata.as_deref().is_some_and(is_music_video))
    {
        MediaKind::MusicVideo
    } else {
        MediaKind::Song
    };
    Ok(Target {
        catalog_id: catalog_id.to_owned(),
        playback: playback_for(&rows, catalog_id, kind, prefer),
        kind,
        twin_of: twin_of.map(str::to_owned),
    })
}

/// Picks the request shape Cider sends for `catalog_id`, given every stored
/// row with that id or catalog id (see the module docs).
pub(crate) fn playback_for(
    rows: &[TrackRecord],
    catalog_id: &str,
    kind: MediaKind,
    prefer: Option<&str>,
) -> PlaybackId {
    if kind == MediaKind::MusicVideo {
        return PlaybackId::catalog(catalog_id);
    }
    let linked: Vec<&str> = rows
        .iter()
        .filter(|row| row.track.id != catalog_id)
        .filter(|row| {
            row.track
                .metadata
                .as_deref()
                .is_some_and(|metadata| plays_as(metadata, catalog_id))
        })
        .map(|row| row.track.id.as_str())
        .collect();
    let library_id = prefer
        .filter(|id| linked.contains(id))
        .or_else(|| linked.first().copied());
    match library_id {
        Some(library_id) => PlaybackId::library(library_id, catalog_id),
        None => PlaybackId::catalog(catalog_id),
    }
}

/// A stored library copy with `playParams`, or a stored catalog row, that is
/// the same recording as `original`.
async fn local_twin(storage: &Storage, original: &Track) -> Result<Option<String>, DownloadError> {
    let key = TwinKey::of_track(original);
    let candidates = storage
        .tracks_with_title_and_artist(&original.title, &original.artist, &original.id)
        .await
        .map_err(|error| store_error(&error))?;
    let mut fallback = None;
    for candidate in candidates {
        let track = &candidate.track;
        if track.metadata.as_deref().is_some_and(is_music_video)
            || !key.matches(&TwinKey::of_track(track))
        {
            continue;
        }
        let Some(catalog_id) = playable_catalog_id(track) else {
            continue;
        };
        // A library copy with `playParams` mirrors what Cider plays; a bare
        // catalog row is just as playable.
        if is_catalog_id(&track.id) {
            fallback.get_or_insert(catalog_id);
        } else {
            return Ok(Some(catalog_id));
        }
    }
    Ok(fallback)
}

/// A catalog search hit that is the same recording as `original`.
async fn remote_twin(
    search: &dyn TwinSearch,
    original: &Track,
) -> Result<Option<String>, DownloadError> {
    let key = TwinKey::of_track(original);
    if !key.is_searchable() {
        return Ok(None);
    }
    let term = format!("{} {}", original.title, original.artist).replace([',', '&'], " ");
    let hits = search.search_songs(&term).await.map_err(search_error)?;
    Ok(hits
        .into_iter()
        .find(|hit| key.matches(&TwinKey::of_candidate(hit)))
        .map(|hit| hit.id))
}

/// The catalog id a stored row plays as: its own numeric id, or the
/// `playParams.catalogId` of a library copy.
fn playable_catalog_id(track: &Track) -> Option<String> {
    if is_catalog_id(&track.id) {
        return Some(track.id.clone());
    }
    let catalog_id = track.catalog_id.as_deref()?;
    track
        .metadata
        .as_deref()
        .is_some_and(|metadata| plays_as(metadata, catalog_id))
        .then(|| catalog_id.to_owned())
}

/// The fields that identify one recording.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TwinKey {
    title: String,
    artist: String,
    album: Option<String>,
    track_number: Option<u32>,
    duration_ms: Option<u32>,
    isrc: Option<String>,
}

impl TwinKey {
    fn of_track(track: &Track) -> Self {
        Self {
            title: normalize(&track.title),
            artist: normalize(&track.artist),
            album: track.album.as_deref().map(normalize),
            track_number: track.track_number,
            duration_ms: track.duration_ms,
            isrc: track.isrc.clone(),
        }
    }

    fn of_candidate(hit: &SongCandidate) -> Self {
        Self {
            title: normalize(&hit.title),
            artist: normalize(&hit.artist),
            album: hit.album.as_deref().map(normalize),
            track_number: hit.track_number,
            duration_ms: hit.duration_ms,
            isrc: hit.isrc.clone(),
        }
    }

    /// Enough is known to match safely (album and length are required).
    fn is_searchable(&self) -> bool {
        !self.title.is_empty() && self.album.is_some() && self.duration_ms.is_some()
    }

    /// Same title, artist and album, lengths within the tolerance, and the
    /// same track number and ISRC wherever both sides know them.
    fn matches(&self, other: &Self) -> bool {
        let (Some(album), Some(other_album)) = (&self.album, &other.album) else {
            return false;
        };
        let (Some(duration), Some(other_duration)) = (self.duration_ms, other.duration_ms) else {
            return false;
        };
        let agree = |a: Option<u32>, b: Option<u32>| a.zip(b).is_none_or(|(a, b)| a == b);
        let isrc_agrees = self
            .isrc
            .as_deref()
            .zip(other.isrc.as_deref())
            .is_none_or(|(a, b)| a.eq_ignore_ascii_case(b));
        self.title == other.title
            && self.artist == other.artist
            && album == other_album
            && duration.abs_diff(other_duration) <= DURATION_TOLERANCE_MS
            && agree(self.track_number, other.track_number)
            && isrc_agrees
    }
}

fn normalize(text: &str) -> String {
    text.trim().to_lowercase()
}

/// A numeric catalog (adam) id.
pub(crate) fn is_catalog_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
}

/// Whether a stored resource carries `playParams.catalogId == catalog_id`.
fn plays_as(metadata: &str, catalog_id: &str) -> bool {
    serde_json::from_str::<Value>(metadata).is_ok_and(|value| {
        value
            .pointer("/attributes/playParams/catalogId")
            .and_then(Value::as_str)
            == Some(catalog_id)
    })
}

/// Whether a stored Apple resource is a music video (`type` of
/// `music-videos` / `library-music-videos`, or `playParams.kind` of
/// `musicVideo`).
pub(crate) fn is_music_video(metadata: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(metadata) else {
        return false;
    };
    let kind = value
        .pointer("/attributes/playParams/kind")
        .and_then(Value::as_str);
    let resource_type = value.get("type").and_then(Value::as_str);
    kind == Some("musicVideo") || resource_type.is_some_and(|t| t.ends_with("music-videos"))
}

fn store_error(error: &StorageError) -> DownloadError {
    DownloadError::transient(format!("local store: {error}"))
}

fn search_error(error: CatalogError) -> DownloadError {
    match error {
        CatalogError::MissingToken(_)
        | CatalogError::Status {
            status: 401 | 403, ..
        } => DownloadError::auth_expired(error.to_string()),
        CatalogError::Status { status, .. } => {
            DownloadError::transient_http(status, None, format!("catalog search: {error}"))
        }
        other => DownloadError::transient(format!("catalog search: {other}")),
    }
}

/// The audio song a music video was cut from, on the same album: offered to
/// the user as "Download album version" (the edit can differ, so it is never
/// substituted silently).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumVersion {
    /// Catalog (adam) id of the song.
    pub catalog_id: String,
    /// Title.
    pub title: String,
    /// Artist.
    pub artist: String,
    /// Album.
    pub album: Option<String>,
    /// Length of the song in milliseconds.
    pub duration_ms: Option<u32>,
    /// Artwork URL template.
    pub artwork_url: Option<String>,
}

/// The [`AlbumVersion`] of the music video `video_id` (its catalog or library
/// id), from what the store knows: a song with the same title and artist on
/// the same album. `None` when `video_id` is not a stored music video or no
/// such song is stored.
///
/// # Errors
///
/// Returns database errors.
pub async fn album_version(
    storage: &Storage,
    video_id: &str,
) -> Result<Option<AlbumVersion>, StorageError> {
    let rows = storage.tracks_by_any_id(video_id).await?;
    let Some(video) = rows
        .iter()
        .find(|row| row.track.metadata.as_deref().is_some_and(is_music_video))
    else {
        return Ok(None);
    };
    let Some(album) = video.track.album.as_deref().map(normalize) else {
        return Ok(None);
    };
    let video_catalog = video.catalog_or_id();
    let candidates = storage
        .tracks_with_title_and_artist(&video.track.title, &video.track.artist, &video.track.id)
        .await?;
    Ok(candidates.into_iter().find_map(|candidate| {
        let track = candidate.track;
        let same_album = track.album.as_deref().map(normalize).as_deref() == Some(album.as_str());
        if !same_album || track.metadata.as_deref().is_some_and(is_music_video) {
            return None;
        }
        let catalog_id = playable_catalog_id(&track)
            .or_else(|| track.catalog_id.clone().filter(|id| is_catalog_id(id)))?;
        (catalog_id != video_catalog).then_some(AlbumVersion {
            catalog_id,
            title: track.title,
            artist: track.artist,
            album: track.album,
            duration_ms: track.duration_ms,
            artwork_url: track.artwork_url,
        })
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Returns canned hits and remembers the searched terms.
    #[derive(Default)]
    struct FakeSearch {
        hits: Vec<SongCandidate>,
        terms: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TwinSearch for FakeSearch {
        async fn search_songs(&self, term: &str) -> Result<Vec<SongCandidate>, CatalogError> {
            if let Ok(mut terms) = self.terms.lock() {
                terms.push(term.to_owned());
            }
            Ok(self.hits.clone())
        }
    }

    fn song(id: &str, catalog_id: Option<&str>, play_params: bool) -> Track {
        let mut track = Track::minimal(id, "LATE NIGHT", "Example Artist & Guest");
        track.catalog_id = catalog_id.map(str::to_owned);
        track.album = Some("$ome $exy $ongs 4 U".into());
        track.duration_ms = Some(195_432);
        track.track_number = Some(15);
        let params = match (play_params, catalog_id) {
            (true, Some(catalog)) => {
                format!(r#","playParams":{{"catalogId":"{catalog}","id":"{id}","kind":"song"}}"#)
            }
            _ => String::new(),
        };
        track.metadata = Some(format!(
            r#"{{"id":"{id}","type":"library-songs","attributes":{{"name":"LATE NIGHT"{params}}}}}"#
        ));
        track
    }

    fn hit(id: &str, duration_ms: u32) -> SongCandidate {
        SongCandidate {
            id: id.into(),
            title: "LATE NIGHT".into(),
            artist: "Example Artist & Guest".into(),
            album: Some("$ome $exy $ongs 4 U".into()),
            duration_ms: Some(duration_ms),
            track_number: Some(15),
            isrc: Some("USLD91772033".into()),
        }
    }

    async fn store(tracks: Vec<Track>) -> Storage {
        let storage = Storage::open_in_memory().unwrap();
        for track in tracks {
            storage.upsert_track(track).await.unwrap();
        }
        storage
    }

    #[tokio::test]
    async fn catalog_id_without_library_copy_should_be_a_catalog_play() {
        let storage = store(vec![]).await;
        let target = resolve(&storage, &FakeSearch::default(), "1443063973")
            .await
            .unwrap();
        assert_eq!(target.playback, PlaybackId::catalog("1443063973"));
    }

    #[tokio::test]
    async fn catalog_id_with_linked_library_copy_should_be_a_library_play() {
        let storage = store(vec![song("i.FakeLibSong0002", Some("6814096109"), true)]).await;
        let target = resolve(&storage, &FakeSearch::default(), "6814096109")
            .await
            .unwrap();
        assert_eq!(
            target.playback,
            PlaybackId::library("i.FakeLibSong0002", "6814096109")
        );
    }

    #[tokio::test]
    async fn library_copy_without_play_params_should_never_be_sent() {
        // A twin link recorded earlier: catalog_id set, but no playParams.
        let storage = store(vec![song("i.FakeLibSong0001", Some("6814096109"), false)]).await;
        let target = resolve(&storage, &FakeSearch::default(), "6814096109")
            .await
            .unwrap();
        assert_eq!(target.playback, PlaybackId::catalog("6814096109"));
    }

    #[tokio::test]
    async fn music_video_should_always_be_a_catalog_play() {
        let mut video = Track::minimal("i.FakeLibVideo001", "KING", "Example Artist");
        video.catalog_id = Some("6782267818".into());
        video.metadata = Some(
            r#"{"type":"library-music-videos","attributes":{"playParams":{"catalogId":"6782267818","kind":"musicVideo"}}}"#
                .into(),
        );
        let storage = store(vec![video]).await;
        let target = resolve(&storage, &FakeSearch::default(), "6782267818")
            .await
            .unwrap();
        assert_eq!(
            (target.kind, target.playback),
            (MediaKind::MusicVideo, PlaybackId::catalog("6782267818"))
        );
    }

    #[tokio::test]
    async fn library_only_item_should_resolve_to_its_library_twin() {
        let storage = store(vec![
            song("i.FakeLibSong0001", None, false),
            song("a.6814096109", Some("6814096109"), true),
        ])
        .await;
        let target = resolve(&storage, &FakeSearch::default(), "i.FakeLibSong0001")
            .await
            .unwrap();
        assert_eq!(
            target,
            Target {
                catalog_id: "6814096109".into(),
                playback: PlaybackId::library("a.6814096109", "6814096109"),
                kind: MediaKind::Song,
                twin_of: Some("i.FakeLibSong0001".into()),
            }
        );
    }

    #[tokio::test]
    async fn library_only_item_should_fall_back_to_a_catalog_search_twin() {
        let storage = store(vec![song("i.FakeLibSong0001", None, false)]).await;
        let search = FakeSearch {
            hits: vec![hit("1", 120_000), hit("6814096109", 195_432)],
            ..FakeSearch::default()
        };
        let target = resolve(&storage, &search, "i.FakeLibSong0001")
            .await
            .unwrap();
        assert_eq!(
            (target.catalog_id.as_str(), target.playback),
            ("6814096109", PlaybackId::catalog("6814096109"))
        );
    }

    #[tokio::test]
    async fn library_only_item_without_twin_should_be_unavailable() {
        let storage = store(vec![song("i.FakeLibSong0001", None, false)]).await;
        let search = FakeSearch {
            hits: vec![hit("1", 120_000)],
            ..FakeSearch::default()
        };
        let error = resolve(&storage, &search, "i.FakeLibSong0001")
            .await
            .unwrap_err();
        assert!(matches!(error, DownloadError::Unavailable { .. }));
    }

    #[tokio::test]
    async fn twin_with_another_track_number_should_not_match() {
        let mut other = song("a.1", Some("1"), true);
        other.track_number = Some(3);
        let storage = store(vec![song("i.FakeLibSong0001", None, false), other]).await;
        let error = resolve(&storage, &FakeSearch::default(), "i.FakeLibSong0001")
            .await
            .unwrap_err();
        assert!(matches!(error, DownloadError::Unavailable { .. }));
    }

    #[tokio::test]
    async fn album_version_should_find_the_song_on_the_same_album() {
        let mut video = Track::minimal("i.FakeLibVideo001", "KING", "Example Artist");
        video.catalog_id = Some("6782267818".into());
        video.album = Some("Example Album (Deluxe)".into());
        video.metadata = Some(r#"{"type":"library-music-videos"}"#.into());
        let mut other_album = Track::minimal("1888707287", "KING", "Example Artist");
        other_album.album = Some("Example Album".into());
        let mut album_song = Track::minimal("6782267800", "KING", "Example Artist");
        album_song.album = Some("Example Album (Deluxe)".into());
        let storage = store(vec![video, other_album, album_song]).await;
        let version = album_version(&storage, "6782267818").await.unwrap();
        assert_eq!(version.map(|v| v.catalog_id).as_deref(), Some("6782267800"));
    }
}
