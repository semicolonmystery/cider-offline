//! Library facade for the UI: local audio import, lyrics, removal, cache
//! maintenance and artwork lookup. Obtain it with
//! [`crate::ProxyHandle::library`] (or [`Library::new`] without a proxy).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hudsucker::hyper::Uri;
use reqwest::header::CONTENT_TYPE;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::artwork::{
    ArtworkError, ArtworkStore, LARGE_EDGE, MAX_ARTWORK_BYTES, StoredArtwork, base_key,
    remove_quietly, sized_url,
};
use crate::config::AppPaths;
use crate::config::Config;
use crate::download::{
    DownloadError, DownloadRequest, DownloaderRegistry, LOCAL_IMPORT, TrackInfo,
};
use crate::lyrics::{
    AmllProvider, AppleLyricsProvider, CommunityProvider, FetchedLyrics, KugouProvider,
    LRCLIB_BASE_URL, LookupMode, LrclibClient, LrclibProvider, LyricsEngine, LyricsError,
    LyricsProvider, LyricsProviderId, LyricsSettings, LyricsStore, MusixmatchProvider,
    NeteaseProvider, ProviderError, TrackOutcome, lrc_to_ttml, plain_to_ttml, ttml_quality,
};
pub use crate::lyrics::{LyricsProgress, LyricsReport};
use crate::storage::{CustomAudio, LyricsSource, Storage, StorageError, TrackRecord};

/// Errors from [`Library`] operations.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// No track with this id (or catalog id) is in the library.
    #[error("unknown track {0:?}")]
    UnknownTrack(String),
    /// No downloader is registered under this id.
    #[error("unknown downloader {0:?}")]
    UnknownDownloader(String),
    /// Database error.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Download / import failed.
    #[error(transparent)]
    Download(#[from] DownloadError),
    /// Lyrics lookup or storage failed.
    #[error(transparent)]
    Lyrics(#[from] LyricsError),
    /// Artwork cleanup failed.
    #[error(transparent)]
    Artwork(#[from] ArtworkError),
    /// A chosen lyrics file had no usable content (neither TTML nor parseable
    /// LRC / plain text).
    #[error("the lyrics file {0} has no usable lyrics")]
    EmptyLyrics(PathBuf),
    /// Every provider that could answer failed (rate limit, server or
    /// network error, rejected request).
    #[error("lyrics lookup failed: {0}")]
    LyricsLookup(String),
    /// A provider failed; the lookup is retried later (see
    /// [`crate::lyrics::FailurePolicy::SkipAndRetryLater`]).
    #[error("lyrics lookup deferred: {} not answering, retried later", providers.iter().map(|p| p.label()).collect::<Vec<_>>().join(", "))]
    LyricsDeferred {
        /// The providers that failed.
        providers: Vec<LyricsProviderId>,
    },
}

/// Library operations shared by the UI and the proxy. Cheap to clone.
#[derive(Clone)]
pub struct Library {
    storage: Storage,
    paths: AppPaths,
    lyrics: Arc<LyricsEngine>,
    artwork: ArtworkStore,
    http: reqwest::Client,
    downloaders: Arc<DownloaderRegistry>,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library")
            .field("paths", &self.paths)
            .field("downloaders", &self.downloaders)
            .finish_non_exhaustive()
    }
}

impl Library {
    /// Library over `storage` and `paths`, using the public LRCLIB instance
    /// and only the built-in [`crate::download::LocalImportDownloader`].
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(storage: Storage, paths: AppPaths) -> Result<Self, LibraryError> {
        Self::with_options(
            storage,
            paths,
            LrclibClient::new(LRCLIB_BASE_URL)?,
            DownloaderRegistry::new(),
        )
    }

    /// Library with an explicit LRCLIB client and downloader registry.
    ///
    /// # Errors
    ///
    /// Currently infallible; kept fallible for future setup steps.
    pub fn with_options(
        storage: Storage,
        paths: AppPaths,
        lrclib: LrclibClient,
        downloaders: DownloaderRegistry,
    ) -> Result<Self, LibraryError> {
        Self::with_options_arc(storage, paths, lrclib, Arc::new(downloaders))
    }

    /// Library sharing a downloader registry with another component.
    ///
    /// # Errors
    ///
    /// Currently infallible; kept fallible for future setup steps.
    pub fn with_options_arc(
        storage: Storage,
        paths: AppPaths,
        lrclib: LrclibClient,
        downloaders: Arc<DownloaderRegistry>,
    ) -> Result<Self, LibraryError> {
        let providers: Vec<Arc<dyn LyricsProvider>> = vec![
            Arc::new(AppleLyricsProvider::new(storage.clone())?),
            Arc::new(CommunityProvider::new()?),
            Arc::new(AmllProvider::new()?),
            Arc::new(MusixmatchProvider::new()?),
            Arc::new(NeteaseProvider::new()?),
            Arc::new(KugouProvider::new()?),
            Arc::new(LrclibProvider::new(lrclib)),
        ];
        Ok(Self {
            lyrics: Arc::new(LyricsEngine::new(
                storage.clone(),
                LyricsStore::new(paths.lyrics_dir()),
                providers,
            )),
            artwork: ArtworkStore::new(paths.artwork_dir(), storage.clone()),
            http: reqwest::Client::new(),
            storage,
            paths,
            downloaders,
        })
    }

    /// Replaces the lyrics providers (normally Apple Music and LRCLIB), e.g.
    /// with test doubles. Settings set before are dropped: call
    /// [`Self::apply_lyrics_settings`] afterwards.
    #[must_use]
    pub fn with_lyrics_providers(mut self, providers: Vec<Arc<dyn LyricsProvider>>) -> Self {
        // Keep the same store (and so its shared index) the proxy may use.
        let store = self.lyrics.store().clone();
        self.lyrics = Arc::new(LyricsEngine::new(self.storage.clone(), store, providers));
        self
    }

    /// The local lyrics files. Clones share the in-memory index of which
    /// songs have lyrics, so writes through this library are seen by the
    /// proxy's lyrics interceptors at once.
    pub fn lyrics_store(&self) -> LyricsStore {
        self.lyrics.store().clone()
    }

    /// Applies the lyrics settings of `config` (providers, order, failure
    /// policy, overwrite, …); the next lookup uses them.
    pub fn apply_lyrics_config(&self, config: &Config) {
        self.apply_lyrics_settings(&config.lyrics, config.overwrite_lyrics);
    }

    /// Applies lyrics settings and the "overwrite existing lyrics" switch.
    pub fn apply_lyrics_settings(&self, settings: &LyricsSettings, overwrite: bool) {
        self.lyrics.configure(settings, overwrite);
    }

    /// The lyrics settings in effect.
    pub fn lyrics_settings(&self) -> LyricsSettings {
        self.lyrics.settings()
    }

    /// Downloaded songs whose lyrics lookup waits for a retry once the
    /// download queue is idle.
    pub fn deferred_download_lyrics(&self) -> usize {
        self.lyrics.deferred_download_count()
    }

    /// Tells the post-download lyrics retry whether the download queue is
    /// idle (no queued or running downloads).
    pub fn set_downloads_idle(&self, idle: bool) {
        self.lyrics.set_downloads_idle(idle);
    }

    /// The local store.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Registered downloader ids (always includes [`LOCAL_IMPORT`]).
    pub fn downloader_ids(&self) -> Vec<&'static str> {
        self.downloaders.ids()
    }

    async fn record(&self, track_id: &str) -> Result<TrackRecord, LibraryError> {
        self.storage
            .find_track(track_id)
            .await?
            .ok_or_else(|| LibraryError::UnknownTrack(track_id.to_owned()))
    }

    fn owns(&self, path: &Path) -> bool {
        path.starts_with(self.paths.media_dir())
    }

    /// Copies `source` into the media directory as the track's local audio
    /// (replacing a previously imported file) and returns the new path.
    ///
    /// # Errors
    ///
    /// [`LibraryError::UnknownTrack`] if the track is not in the library;
    /// download errors for unsupported formats or I/O failures.
    pub async fn import_audio(
        &self,
        track_id: &str,
        source: &Path,
    ) -> Result<PathBuf, LibraryError> {
        self.download(LOCAL_IMPORT, track_id, Some(source)).await
    }

    /// Imports a user-chosen audio file for a **custom** song: copies it into
    /// the media directory (atomically — temp file + rename, via
    /// [`crate::download::LocalImportDownloader`]) named by the song id, removes
    /// any previously imported file it owns, and records it as the song's
    /// local-copy audio ([`CustomAudio::LocalCopy`]). Returns the stored path.
    ///
    /// # Errors
    ///
    /// [`LibraryError::UnknownTrack`] if the id is not a custom song; download
    /// errors for unsupported formats or I/O failures.
    pub async fn import_custom_audio(
        &self,
        track_id: &str,
        source: &Path,
    ) -> Result<PathBuf, LibraryError> {
        let record = self.record(track_id).await?;
        if !record.track.is_custom {
            return Err(LibraryError::UnknownTrack(track_id.to_owned()));
        }
        let downloader = self
            .downloaders
            .get(LOCAL_IMPORT)
            .ok_or_else(|| LibraryError::UnknownDownloader(LOCAL_IMPORT.to_owned()))?;
        let track = &record.track;
        // A custom song has no catalog id; key the file on its `local-<n>` id.
        let request = DownloadRequest::new(track.id.clone(), self.paths.media_dir())
            .with_info(TrackInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration_ms: track.duration_ms,
            })
            .with_source(Some(source.to_path_buf()));
        let path = downloader.fetch(&request).await?;
        if let Some(old) = &track.audio_path
            && *old != path
            && self.owns(old)
        {
            remove_quietly(old).await;
        }
        self.storage
            .set_custom_track_audio(&track.id, CustomAudio::LocalCopy(path.clone()))
            .await?;
        tracing::info!(track = %track.id, "custom audio imported");
        Ok(path)
    }

    /// Produces local audio for a track with the downloader registered as
    /// `downloader_id` and records it as the track's audio file.
    ///
    /// # Errors
    ///
    /// Unknown track / downloader, or the downloader's error.
    pub async fn download(
        &self,
        downloader_id: &str,
        track_id: &str,
        source: Option<&Path>,
    ) -> Result<PathBuf, LibraryError> {
        let downloader = self
            .downloaders
            .get(downloader_id)
            .ok_or_else(|| LibraryError::UnknownDownloader(downloader_id.to_owned()))?;
        let record = self.record(track_id).await?;
        let track = &record.track;
        // Key the download on the catalog (adam) id when the track has one, so
        // files and rows match the auto-cache path; fall back to the row id for
        // catalog-native tracks.
        let catalog_id = record.catalog_or_id().to_owned();
        let request = DownloadRequest::new(catalog_id, self.paths.media_dir())
            .with_info(TrackInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration_ms: track.duration_ms,
            })
            .with_source(source.map(Path::to_path_buf));
        let path = downloader.fetch(&request).await?;
        if let Some(old) = &track.audio_path
            && *old != path
            && self.owns(old)
        {
            remove_quietly(old).await;
        }
        self.storage
            .set_track_audio(&track.id, Some(path.clone()))
            .await?;
        tracing::info!(track = %track.id, downloader = downloader_id, "local audio recorded");
        Ok(path)
    }

    /// Asks one lyrics provider about a library track, once, without
    /// storing anything: the diagnostics behind `--lyrics-check --provider`.
    ///
    /// # Errors
    ///
    /// [`LibraryError::UnknownTrack`] (or a storage error) when the track is
    /// not in the library, [`LibraryError::LyricsLookup`] when `provider` is
    /// not registered. The provider's own answer, including its failure, is
    /// the inner result.
    pub async fn probe_lyrics(
        &self,
        provider: LyricsProviderId,
        track_id: &str,
    ) -> Result<Result<Option<FetchedLyrics>, ProviderError>, LibraryError> {
        let record = self
            .storage
            .find_track(track_id)
            .await?
            .ok_or_else(|| LibraryError::UnknownTrack(track_id.to_owned()))?;
        self.lyrics
            .probe(provider, &record)
            .await
            .ok_or_else(|| LibraryError::LyricsLookup(format!("{provider} is not available")))
    }

    /// Looks lyrics up for one track now, asking the enabled providers in
    /// the configured order and storing the best lyrics found.
    ///
    /// Tracks with custom lyrics (imported by the user) always keep them
    /// (the result is then [`LyricsSource::Custom`]). Tracks that already
    /// have Apple lyrics on disk keep them (the result
    /// is then [`LyricsSource::Apple`] without any network request). Other
    /// existing lyrics are replaced by what is found. Returns
    /// [`LyricsSource::None`] when no provider has lyrics (an earlier file is
    /// kept in that case). With a skip policy a failing provider is not
    /// waited for.
    ///
    /// # Errors
    ///
    /// Unknown track, [`LibraryError::LyricsLookup`] when providers failed,
    /// or file / database errors.
    pub async fn refresh_lyrics(&self, track_id: &str) -> Result<LyricsSource, LibraryError> {
        let record = self.record(track_id).await?;
        match self
            .lyrics
            .lookup_record(&record, LookupMode::Explicit, None)
            .await?
        {
            TrackOutcome::Found(source, ..) | TrackOutcome::Kept(source, _) => Ok(source),
            TrackOutcome::NotFound => Ok(LyricsSource::None),
            TrackOutcome::Failed(reason) => Err(LibraryError::LyricsLookup(reason)),
            TrackOutcome::Deferred(providers) => Err(LibraryError::LyricsDeferred { providers }),
        }
    }

    /// Fetches and stores lyrics for a freshly downloaded track, unless it
    /// already has local lyrics (and "overwrite" is off) or fetching after
    /// downloads is turned off.
    ///
    /// Uses the same lookup as [`Self::refresh_lyrics`] when the track is in
    /// the library; a catalog-only download (no library row) is looked up by
    /// the queued `info` and stored under `catalog_id`, which is the id the
    /// proxy serves lyrics by. Returns `None` when skipped, otherwise the
    /// resulting source. With "skip and retry at the end", a failed lookup
    /// joins a list that is retried once the download queue is idle.
    ///
    /// # Errors
    ///
    /// [`LibraryError::LyricsDeferred`] when the lookup was deferred,
    /// [`LibraryError::LyricsLookup`] when it failed, or file / database
    /// errors.
    pub async fn ensure_download_lyrics(
        &self,
        catalog_id: &str,
        info: &TrackInfo,
    ) -> Result<Option<LyricsSource>, LibraryError> {
        match self.lyrics.after_download(catalog_id, info).await? {
            None => Ok(None),
            Some(TrackOutcome::Found(source, ..) | TrackOutcome::Kept(source, _)) => {
                Ok(Some(source))
            }
            Some(TrackOutcome::NotFound) => Ok(Some(LyricsSource::None)),
            Some(TrackOutcome::Failed(reason)) => Err(LibraryError::LyricsLookup(reason)),
            Some(TrackOutcome::Deferred(providers)) => {
                Err(LibraryError::LyricsDeferred { providers })
            }
        }
    }

    /// Imports a user-chosen lyrics file for a track: reads it, detects TTML vs
    /// LRC / plain text (converting LRC and plain text to Apple-style TTML), and
    /// stores it as the track's custom lyrics (`<id>.custom.ttml`, keyed, like
    /// [`Self::refresh_lyrics`], by the track's catalog id when known). The
    /// track's `lyrics_source` becomes [`LyricsSource::Custom`]: from then on
    /// Cider gets these lyrics on both lyrics endpoints, online and offline,
    /// and neither Apple's answers nor lyrics lookups replace them (importing
    /// another file does). Returns the resulting source.
    ///
    /// # Errors
    ///
    /// [`LibraryError::UnknownTrack`] for an unknown id, [`LibraryError::EmptyLyrics`]
    /// when the file has no usable lyrics, or file / database errors.
    pub async fn import_lyrics(
        &self,
        track_id: &str,
        source: &Path,
    ) -> Result<LyricsSource, LibraryError> {
        let record = self.record(track_id).await?;
        let lyrics_id = record.catalog_or_id().to_owned();
        let text = tokio::fs::read_to_string(source)
            .await
            .map_err(LyricsError::Io)?;
        // Strip a leading BOM before sniffing the format.
        let trimmed = text.trim_start_matches('\u{feff}');
        let ttml = if trimmed.trim_start().starts_with('<') {
            // Already an XML/TTML document: store it verbatim.
            trimmed.to_owned()
        } else {
            // Timed LRC first, then plain text.
            let duration = record.track.duration_ms.map(u64::from);
            lrc_to_ttml(trimmed, duration)
                .or_else(|| plain_to_ttml(trimmed))
                .ok_or_else(|| LibraryError::EmptyLyrics(source.to_path_buf()))?
        };
        if ttml.trim().is_empty() {
            return Err(LibraryError::EmptyLyrics(source.to_path_buf()));
        }
        self.lyrics.store().write_custom(&lyrics_id, &ttml).await?;
        let quality = ttml_quality(&ttml);
        for id in [lyrics_id.as_str(), record.track.id.as_str()] {
            self.storage
                .set_lyrics_state(id, LyricsSource::Custom, quality)
                .await?;
        }
        tracing::info!(track = %record.track.id, "custom lyrics imported");
        Ok(LyricsSource::Custom)
    }

    /// Looks lyrics up for every track without lyrics. See
    /// [`Self::fetch_lyrics_for`].
    ///
    /// # Errors
    ///
    /// Database errors listing the tracks.
    pub async fn fetch_missing_lyrics(&self) -> Result<LyricsReport, LibraryError> {
        self.fetch_missing_lyrics_with(&|_| {}).await
    }

    /// [`Self::fetch_missing_lyrics`] reporting progress to `progress`.
    ///
    /// # Errors
    ///
    /// Database errors listing the tracks.
    pub async fn fetch_missing_lyrics_with(
        &self,
        progress: &(dyn Fn(&LyricsProgress) + Send + Sync),
    ) -> Result<LyricsReport, LibraryError> {
        let missing = self
            .storage
            .tracks_with_lyrics_source(LyricsSource::None)
            .await?;
        let ids: Vec<String> = missing.into_iter().map(|record| record.track.id).collect();
        Ok(self.fetch_lyrics_for_with(&ids, progress).await)
    }

    /// Looks lyrics up for `track_ids` one at a time. Tracks with lyrics
    /// keep them unless "overwrite lyrics" is on. Provider requests are paced
    /// (LRCLIB: the configured spacing) and failures follow the configured
    /// policy: wait and retry, skip and retry after the batch, or skip.
    pub async fn fetch_lyrics_for(&self, track_ids: &[String]) -> LyricsReport {
        self.fetch_lyrics_for_with(track_ids, &|_| {}).await
    }

    /// [`Self::fetch_lyrics_for`] reporting progress to `progress`.
    pub async fn fetch_lyrics_for_with(
        &self,
        track_ids: &[String],
        progress: &(dyn Fn(&LyricsProgress) + Send + Sync),
    ) -> LyricsReport {
        self.lyrics.run_batch(track_ids, progress).await
    }

    /// Removes a track from the library, deleting the audio file it owns
    /// (inside the media directory) and its local lyrics. User files
    /// referenced by track mappings are never deleted, and mappings are kept.
    /// Removing an unknown track is a no-op.
    ///
    /// # Errors
    ///
    /// Database errors.
    pub async fn remove_track(&self, track_id: &str) -> Result<(), LibraryError> {
        let Some(record) = self.storage.find_track(track_id).await? else {
            return Ok(());
        };
        if let Some(audio) = &record.track.audio_path
            && self.owns(audio)
        {
            remove_quietly(audio).await;
        }
        self.lyrics.store().remove(&record.track.id).await;
        if let Some(catalog_id) = &record.catalog_id {
            self.lyrics.store().remove(catalog_id).await;
        }
        self.storage.delete_track(&record.track.id).await?;
        Ok(())
    }

    /// Links audio files already in the media directory to tracks without
    /// recorded audio (see [`crate::media`]); returns how many rows were
    /// linked. Files are never moved or deleted.
    pub async fn link_existing_media(&self) -> usize {
        crate::media::link_existing_media(&self.storage, &self.paths.media_dir()).await
    }

    /// Deletes all cached HTTP responses and artwork files. Library
    /// metadata, local audio and lyrics are kept.
    ///
    /// # Errors
    ///
    /// Database errors.
    pub async fn clear_cache(&self) -> Result<(), LibraryError> {
        let responses = self.storage.clear_http_cache().await?;
        let images = self.artwork.clear().await?;
        tracing::info!(responses, images, "cache cleared");
        Ok(())
    }

    /// Resolves an artwork URL — concrete (`…/600x600bb.jpg`) or an Apple
    /// template (`…/{w}x{h}bb.jpg`, as stored in `Track::artwork_url` /
    /// `Playlist::artwork_url`) — to a cached image file, preferring the
    /// exact size and otherwise the largest cached size of the same image.
    /// Returns `None` if nothing usable is cached or the URL is invalid.
    pub async fn artwork_path(&self, url: &str) -> Option<PathBuf> {
        let encoded = url.replace('{', "%7B").replace('}', "%7D");
        let uri: Uri = encoded.parse().ok()?;
        match self.artwork.lookup(&uri).await {
            Ok(found) => found.map(|a| a.path),
            Err(error) => {
                tracing::warn!(%error, "artwork lookup failed");
                None
            }
        }
    }

    /// Resolves an artwork URL to a cached image file for the UI at the large
    /// cover size ([`crate::artwork::LARGE_EDGE`]); see
    /// [`Self::ensure_artwork_sized`].
    pub async fn ensure_artwork(&self, url: &str) -> Option<PathBuf> {
        self.ensure_artwork_sized(url, LARGE_EDGE)
            .await
            .map(|found| found.path)
    }

    /// Resolves an artwork URL (concrete or an Apple `{w}x{h}` template) to a
    /// cached image of at least `edge`×`edge` pixels, fetching that size from
    /// the CDN only when no large-enough copy of the same image is cached.
    ///
    /// Every song of an album shares one cache identity (the URL without its
    /// size segment, see [`crate::artwork::base_key`]), so a cover is stored
    /// once and reused by every row and card; a smaller cached copy is never
    /// returned while a sharper one can be fetched. Returns `None` if nothing
    /// is cached and the fetch failed (the UI then shows its placeholder).
    pub async fn ensure_artwork_sized(&self, url: &str, edge: u32) -> Option<ResolvedArtwork> {
        let concrete = sized_url(url, edge);
        let uri: Uri = concrete.parse().ok()?;
        let cached = match self.artwork.lookup_sized(&uri, edge).await {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "artwork lookup failed");
                None
            }
        };
        let wanted = u64::from(edge) * u64::from(edge);
        // `0` = unknown size (non-mzstatic URL): trust it.
        if let Some(found) = &cached
            && (found.pixels == 0 || found.pixels >= wanted)
        {
            return Some(ResolvedArtwork::from_stored(found, false));
        }
        match self.fetch_artwork(&uri, &concrete).await {
            Some(stored) => Some(ResolvedArtwork::from_stored(&stored, true)),
            // Offline / CDN error: a smaller cached copy beats a placeholder.
            None => cached.map(|found| ResolvedArtwork::from_stored(&found, false)),
        }
    }

    /// Downloads one concrete artwork URL into the artwork store.
    async fn fetch_artwork(&self, uri: &Uri, url: &str) -> Option<StoredArtwork> {
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            return None;
        }
        let response = match self.http.get(url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::debug!(status = %response.status(), "artwork fetch not ok");
                return None;
            }
            Err(error) => {
                tracing::debug!(%error, "artwork fetch failed");
                return None;
            }
        };
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map_or_else(
                || "image/jpeg".to_owned(),
                |value| value.split(';').next().unwrap_or(value).trim().to_owned(),
            );
        let body = response.bytes().await.ok()?;
        if body.is_empty() || body.len() as u64 > MAX_ARTWORK_BYTES {
            return None;
        }
        if let Err(error) = self.artwork.store(uri, &content_type, body).await {
            tracing::warn!(%error, "artwork store failed");
            return None;
        }
        self.artwork.lookup(uri).await.ok().flatten()
    }

    /// Downloads every requested cover that is not cached at its size yet,
    /// at most `concurrency` at a time, so the UI later opens collections
    /// without touching the network. Requests are deduplicated by image
    /// identity and size, so the songs of one album cost one request.
    /// Stops early (keeping completed files) when `cancel` fires.
    pub async fn prefetch_artwork(
        &self,
        requests: Vec<ArtworkRequest>,
        concurrency: usize,
        cancel: CancellationToken,
    ) -> PrefetchReport {
        let mut seen = HashSet::new();
        let unique: Vec<ArtworkRequest> = requests
            .into_iter()
            .filter(|request| seen.insert((identity_key(&request.url), request.edge)))
            .collect();
        let mut report = PrefetchReport {
            requested: unique.len(),
            ..PrefetchReport::default()
        };
        let limit = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut tasks = JoinSet::new();
        for request in unique {
            let library = self.clone();
            let limit = Arc::clone(&limit);
            tasks.spawn(async move {
                let _permit = limit.acquire_owned().await;
                library
                    .ensure_artwork_sized(&request.url, request.edge)
                    .await
                    .map(|found| found.fetched)
            });
        }
        loop {
            let joined = tokio::select! {
                () = cancel.cancelled() => {
                    report.cancelled = true;
                    break;
                }
                joined = tasks.join_next() => joined,
            };
            match joined {
                None => break,
                Some(Ok(Some(true))) => report.fetched += 1,
                Some(Ok(Some(false))) => report.cached += 1,
                Some(Ok(None)) => report.failed += 1,
                Some(Err(error)) => {
                    tracing::debug!(%error, "artwork prefetch task failed");
                    report.failed += 1;
                }
            }
        }
        // Dropping the JoinSet aborts whatever is still running.
        report
    }
}

/// Size-independent cache identity of an artwork URL or template (see
/// [`crate::artwork::base_key`]); the URL itself when it does not parse.
pub fn identity_key(url: &str) -> String {
    url.replace('{', "%7B")
        .replace('}', "%7D")
        .parse::<Uri>()
        .map_or_else(|_| url.to_owned(), |uri| base_key(&uri))
}

/// One cover to prefetch: an artwork URL (or template) and the edge in
/// pixels it is displayed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkRequest {
    /// Artwork URL or `{w}x{h}` template.
    pub url: String,
    /// Wanted edge in pixels.
    pub edge: u32,
}

/// Outcome of [`Library::prefetch_artwork`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefetchReport {
    /// Distinct (image, size) requests after deduplication.
    pub requested: usize,
    /// Already cached at a sufficient size (no network).
    pub cached: usize,
    /// Downloaded now.
    pub fetched: usize,
    /// Could not be fetched.
    pub failed: usize,
    /// The run was cancelled before finishing.
    pub cancelled: bool,
}

/// A cover resolved by [`Library::ensure_artwork_sized`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtwork {
    /// The image file.
    pub path: PathBuf,
    /// `width * height` of the stored copy as parsed from its URL (`0` when
    /// unknown).
    pub pixels: u64,
    /// Whether it was downloaded by this call (network) rather than cached.
    pub fetched: bool,
}

impl ResolvedArtwork {
    fn from_stored(stored: &StoredArtwork, fetched: bool) -> Self {
        Self {
            path: stored.path.clone(),
            pixels: stored.pixels,
            fetched,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::LyricsKind;
    use crate::storage::{CatalogUpdate, TrackMetadata};
    use bytes::Bytes;

    async fn library(dir: &tempfile::TempDir) -> Library {
        let paths = AppPaths::under(dir.path());
        paths.ensure_dirs().unwrap();
        let storage = Storage::open_in_memory().unwrap();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![TrackMetadata {
                    id: "i.lib".to_owned(),
                    catalog_id: Some("100".to_owned()),
                    title: "Title".to_owned(),
                    artist: "Artist".to_owned(),
                    album: None,
                    duration_ms: Some(10_000),
                    artwork_url: None,
                    metadata: None,
                    ..TrackMetadata::default()
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let library = Library::with_options(
            storage,
            paths,
            // Unroutable: tests must not reach the network.
            LrclibClient::new("http://127.0.0.1:9").unwrap(),
            DownloaderRegistry::new(),
        )
        .unwrap();
        // Fail fast on the unroutable LRCLIB instead of retry passes.
        let settings = LyricsSettings {
            on_failure: crate::lyrics::FailurePolicy::Skip,
            ..LyricsSettings::default()
        };
        library.apply_lyrics_settings(&settings, false);
        library
    }

    fn source(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("pick.flac");
        std::fs::write(&path, b"flac").unwrap();
        path
    }

    #[tokio::test]
    async fn import_should_record_audio_path() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let path = library.import_audio("100", &source(&dir)).await.unwrap();
        let track = library.storage().get_track("i.lib").await.unwrap().unwrap();
        assert_eq!(track.audio_path, Some(path));
    }

    #[tokio::test]
    async fn import_custom_audio_should_copy_and_record_local_copy() {
        use crate::storage::{CustomAudioMode, NewCustomTrack};

        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let id = library
            .storage()
            .create_custom_track(NewCustomTrack {
                title: "Custom".to_owned(),
                artist: "Me".to_owned(),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let path = library
            .import_custom_audio(&id, &source(&dir))
            .await
            .unwrap();
        assert!(path.starts_with(library.paths.media_dir()));
        let track = library.storage().get_track(&id).await.unwrap().unwrap();
        assert_eq!(track.audio_mode, Some(CustomAudioMode::LocalCopy));
        assert_eq!(track.audio_path, Some(path));
    }

    #[tokio::test]
    async fn import_custom_audio_for_real_track_should_fail() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let error = library
            .import_custom_audio("i.lib", &source(&dir))
            .await
            .unwrap_err();
        assert!(matches!(error, LibraryError::UnknownTrack(_)));
    }

    #[tokio::test]
    async fn import_for_unknown_track_should_fail() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let error = library
            .import_audio("nope", &source(&dir))
            .await
            .unwrap_err();
        assert!(matches!(error, LibraryError::UnknownTrack(_)));
    }

    #[tokio::test]
    async fn remove_should_delete_owned_audio_and_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let audio = library.import_audio("i.lib", &source(&dir)).await.unwrap();
        library
            .lyrics
            .store()
            .write("100", LyricsKind::Line, "<tt/>")
            .await
            .unwrap();
        library.remove_track("i.lib").await.unwrap();
        assert!(!audio.exists() && !library.lyrics.store().exists("100").await);
    }

    #[tokio::test]
    async fn remove_should_keep_user_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let picked = source(&dir);
        library.import_audio("i.lib", &picked).await.unwrap();
        library.remove_track("i.lib").await.unwrap();
        assert!(picked.exists());
    }

    #[tokio::test]
    async fn refresh_should_keep_apple_lyrics_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        library
            .lyrics
            .store()
            .write("100", LyricsKind::Syllable, "<tt/>")
            .await
            .unwrap();
        library
            .storage()
            .set_track_lyrics_source("i.lib", LyricsSource::Apple)
            .await
            .unwrap();
        assert_eq!(
            library.refresh_lyrics("i.lib").await.unwrap(),
            LyricsSource::Apple
        );
    }

    fn info() -> TrackInfo {
        TrackInfo {
            title: "Title".to_owned(),
            artist: "Artist".to_owned(),
            album: None,
            duration_ms: Some(10_000),
        }
    }

    #[tokio::test]
    async fn download_lyrics_should_skip_track_with_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        library
            .lyrics
            .store()
            .write("100", LyricsKind::Line, "<tt/>")
            .await
            .unwrap();
        // Unroutable LRCLIB: reaching the network would be an error.
        let outcome = library
            .ensure_download_lyrics("100", &info())
            .await
            .unwrap();
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn download_lyrics_should_skip_catalog_only_track_with_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        library
            .lyrics
            .store()
            .write("777", LyricsKind::Syllable, "<tt/>")
            .await
            .unwrap();
        let outcome = library
            .ensure_download_lyrics("777", &info())
            .await
            .unwrap();
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn download_lyrics_should_look_up_track_without_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        // No lyrics on disk, so it must try LRCLIB (unroutable here → error).
        assert!(
            library
                .ensure_download_lyrics("100", &info())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn import_lyrics_should_store_ttml_and_mark_source() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let lrc = dir.path().join("words.lrc");
        std::fs::write(&lrc, "[00:01.00]Hello\n[00:03.00]World").unwrap();
        let source = library.import_lyrics("i.lib", &lrc).await.unwrap();
        let stored = library.storage.get_track("i.lib").await.unwrap().unwrap();
        assert_eq!(
            (source, stored.lyrics_source),
            (LyricsSource::Custom, LyricsSource::Custom)
        );
    }

    async fn with_custom_lyrics(dir: &tempfile::TempDir) -> Library {
        let library = library(dir).await;
        // Provider lyrics first, then the user's own file.
        library
            .lyrics
            .store()
            .write("100", LyricsKind::Syllable, "<tt>apple</tt>")
            .await
            .unwrap();
        let lrc = dir.path().join("mine.lrc");
        std::fs::write(&lrc, "[00:01.00]Mine").unwrap();
        library.import_lyrics("i.lib", &lrc).await.unwrap();
        library
    }

    #[tokio::test]
    async fn explicit_refetch_should_keep_custom_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = with_custom_lyrics(&dir).await;
        // No network is reached: LRCLIB is unroutable here and would fail.
        let source = library.refresh_lyrics("i.lib").await.unwrap();
        let served = library
            .lyrics
            .store()
            .read("100", LyricsKind::Syllable)
            .await
            .map(|(ttml, _)| ttml.contains("Mine"));
        assert_eq!((source, served), (LyricsSource::Custom, Some(true)));
    }

    #[tokio::test]
    async fn refetch_with_overwrite_should_keep_custom_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = with_custom_lyrics(&dir).await;
        library.apply_lyrics_settings(&LyricsSettings::default(), true);
        let report = library.fetch_lyrics_for(&["i.lib".to_owned()]).await;
        let stored = library.storage.get_track("i.lib").await.unwrap().unwrap();
        assert_eq!(
            (report.kept, stored.lyrics_source),
            (1, LyricsSource::Custom)
        );
    }

    #[tokio::test]
    async fn download_lyrics_should_keep_custom_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let library = with_custom_lyrics(&dir).await;
        library.apply_lyrics_settings(&LyricsSettings::default(), true);
        let outcome = library
            .ensure_download_lyrics("100", &info())
            .await
            .unwrap();
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn import_lyrics_should_write_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let lrc = dir.path().join("words.lrc");
        std::fs::write(&lrc, "[00:01.00]Hello").unwrap();
        library.import_lyrics("i.lib", &lrc).await.unwrap();
        // Lyrics are keyed by the catalog id ("100") for this library track.
        assert!(library.lyrics.store().exists("100").await);
    }

    #[tokio::test]
    async fn import_lyrics_should_reject_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let empty = dir.path().join("empty.lrc");
        std::fs::write(&empty, "   \n  ").unwrap();
        let error = library.import_lyrics("i.lib", &empty).await.unwrap_err();
        assert!(matches!(error, LibraryError::EmptyLyrics(_)));
    }

    #[tokio::test]
    async fn fetch_missing_should_skip_failures() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        // LRCLIB is unreachable, so the lookup fails and is skipped.
        assert_eq!(library.fetch_missing_lyrics().await.unwrap().found, 0);
    }

    #[tokio::test]
    async fn artwork_template_should_resolve_cached_size() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let concrete: Uri = "https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/600x600bb.jpg"
            .parse()
            .unwrap();
        library
            .artwork
            .store(&concrete, "image/jpeg", Bytes::from_static(b"img"))
            .await
            .unwrap();
        let path = library
            .artwork_path("https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/{w}x{h}bb.jpg")
            .await;
        assert!(path.is_some());
    }

    #[tokio::test]
    async fn clear_cache_should_remove_artwork() {
        let dir = tempfile::tempdir().unwrap();
        let library = library(&dir).await;
        let concrete: Uri = "https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/600x600bb.jpg"
            .parse()
            .unwrap();
        library
            .artwork
            .store(&concrete, "image/jpeg", Bytes::from_static(b"img"))
            .await
            .unwrap();
        library.clear_cache().await.unwrap();
        assert!(
            library
                .artwork_path("https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/600x600bb.jpg")
                .await
                .is_none()
        );
    }
}
