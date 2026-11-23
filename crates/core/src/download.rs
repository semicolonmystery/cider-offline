//! Downloader plug-in point: how a track gets a local audio file.
//!
//! The app ships only [`LocalImportDownloader`], which copies an audio file
//! the user picked into the media directory. No content acquisition, DRM,
//! license or decryption logic exists here, and none may be added.
//!
//! # Plugging in another implementation
//!
//! Implement [`Downloader`] and register it before starting the proxy:
//!
//! ```no_run
//! use std::path::PathBuf;
//! use cider_offline_core::download::{DownloadError, DownloadRequest, Downloaded, Downloader};
//! use cider_offline_core::storage::AudioVariant;
//! use cider_offline_core::{Config, ProxyHandle, async_trait};
//!
//! /// Example: picks files from a folder the user keeps in sync themselves.
//! struct FolderDownloader { folder: PathBuf }
//!
//! #[async_trait]
//! impl Downloader for FolderDownloader {
//!     fn id(&self) -> &'static str { "folder" }
//!
//!     async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
//!         let source = self.folder.join(format!("{}.m4a", request.playback.adam_id()));
//!         tokio::fs::copy(&source, &request.output_path).await?;
//!         Ok(Downloaded::new(request.output_path.clone(), AudioVariant::Stereo))
//!     }
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let handle = ProxyHandle::builder(Config::default())
//!     .downloader(FolderDownloader { folder: "/music".into() })
//!     .start()
//!     .await?;
//! let path = handle.library().download("folder", "1594405597", None).await?;
//! # let _ = path; Ok(()) }
//! ```
//!
//! [`crate::library::Library::download`] then records the returned path as
//! the track's local audio, and playback / offline mode pick it up.
//!
//! The Widevine downloader in `crates/download` plugs in here too, through
//! the [`Downloader`] trait, whose docs spell out the contract (request
//! shapes, typed errors, progress, capabilities). The "How it works" chapter
//! of the user guide gives the overview.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::media::{AUDIO_EXTENSIONS, is_valid_media_id};
use crate::storage::AudioVariant;

/// Id of [`LocalImportDownloader`] in the registry.
pub const LOCAL_IMPORT: &str = "local-import";

/// Stem suffix of a spatial file: `<adam id>.spatial.m4a`.
pub const SPATIAL_FILE_SUFFIX: &str = ".spatial";

/// File name a downloader writes for `variant` of the song `adam_id`:
/// `<adam id>.m4a` for [`AudioVariant::Stereo`], `<adam id>.spatial.m4a` for
/// [`AudioVariant::Spatial`].
#[must_use]
pub fn media_file_name(adam_id: &str, variant: AudioVariant) -> String {
    match variant {
        AudioVariant::Stereo => format!("{adam_id}.m4a"),
        AudioVariant::Spatial => format!("{adam_id}{SPATIAL_FILE_SUFFIX}.m4a"),
    }
}

/// Errors from a [`Downloader`].
///
/// Each variant tells the [`crate::download_queue::DownloadQueue`] what to do
/// next, so a downloader never retries or sleeps itself:
///
/// | variant | queue behaviour |
/// | --- | --- |
/// | [`Unavailable`](Self::Unavailable) | "Can't download yet", never retried |
/// | [`Transient`](Self::Transient) | retried with backoff (at least `retry_after`) |
/// | [`AuthExpired`](Self::AuthExpired) | failed, not retried: the user re-signs in through Cider |
/// | [`VariantUnavailable`](Self::VariantUnavailable) | the song has no such version; the queue falls back or skips it |
/// | [`Cancelled`](Self::Cancelled) | the row keeps the state the user chose (paused / removed) |
/// | [`Io`](Self::Io) | retried with backoff |
/// | [`Other`](Self::Other) | retried with backoff |
///
/// Build the struct variants with their constructors
/// ([`unavailable`](Self::unavailable), [`transient`](Self::transient),
/// [`transient_http`](Self::transient_http),
/// [`auth_expired`](Self::auth_expired)); they are `#[non_exhaustive]` so
/// fields can be added without breaking downloaders.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DownloadError {
    /// This downloader needs a user-chosen source file and none was given.
    #[error("a source file is required")]
    SourceRequired,
    /// The file type is not a supported audio format.
    #[error("unsupported audio file type: {0}")]
    UnsupportedFormat(PathBuf),
    /// The track id cannot be used as a file name.
    #[error("invalid track id {0:?}")]
    InvalidTrackId(String),
    /// File system error. Retried with backoff.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The downloader cannot produce audio for this item with its current
    /// approach (for example Apple returned no usable audio asset). Retrying
    /// would not help: the queue shows "Can't download yet" and never retries
    /// it automatically.
    #[error("can't download yet: {reason}")]
    #[non_exhaustive]
    Unavailable {
        /// User-facing reason; include what Apple returned when known.
        reason: String,
    },
    /// A temporary failure that may succeed later: throttling (HTTP 429), a
    /// 5xx, an empty or non-JSON license body, a network error. The queue
    /// retries it with exponential backoff and waits at least `retry_after`.
    #[error("{message}")]
    #[non_exhaustive]
    Transient {
        /// What failed, for the log and the row ("license HTTP 429, 0 bytes").
        message: String,
        /// HTTP status of the failing response, when there was one.
        status: Option<u16>,
        /// Apple's `Retry-After`, when it sent one.
        retry_after: Option<Duration>,
    },
    /// Apple rejected the captured tokens (HTTP 401/403, `failureType 2002`,
    /// "Your session has ended", or no tokens captured yet). Not retried: the
    /// user plays a song in Cider while online so fresh tokens are captured,
    /// then retries.
    #[error("Apple Music sign-in expired: {message}")]
    #[non_exhaustive]
    AuthExpired {
        /// What Apple returned.
        message: String,
    },
    /// The song exists but not in the requested [`DownloadRequest::variant`]
    /// (a song without a Dolby Atmos mix has no spatial version). Never
    /// retried; the queue falls back to another variant or skips this one,
    /// depending on the download format.
    #[error("this song has no {variant} version")]
    #[non_exhaustive]
    VariantUnavailable {
        /// The variant that was asked for.
        variant: AudioVariant,
    },
    /// The download stopped because [`DownloadRequest::cancel`] fired.
    #[error("download cancelled")]
    Cancelled,
    /// Implementation-specific failure that fits none of the typed variants.
    /// Retried with backoff; prefer a typed variant when one applies.
    #[error("{0}")]
    Other(String),
}

impl DownloadError {
    /// [`DownloadError::Unavailable`] with `reason`.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    /// [`DownloadError::Transient`] without an HTTP status (for example a
    /// network error).
    pub fn transient(message: impl Into<String>) -> Self {
        Self::Transient {
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }

    /// [`DownloadError::Transient`] for an HTTP response with `status` and
    /// Apple's optional `Retry-After`.
    pub fn transient_http(
        status: u16,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        Self::Transient {
            message: message.into(),
            status: Some(status),
            retry_after,
        }
    }

    /// [`DownloadError::AuthExpired`] with what Apple returned.
    pub fn auth_expired(message: impl Into<String>) -> Self {
        Self::AuthExpired {
            message: message.into(),
        }
    }

    /// [`DownloadError::VariantUnavailable`] for `variant`.
    pub fn variant_unavailable(variant: AudioVariant) -> Self {
        Self::VariantUnavailable { variant }
    }
}

/// Coarse phase of a running download, reported through
/// [`DownloadProgress::stage`]. The queue maps each phase to a progress band
/// so the UI ring advances even when no byte counts are reported.
///
/// Together with [`crate::storage::DownloadStatus`] this is the queue's state
/// machine: `Queued → Requesting → License → Fetching → Decrypting → Saving →
/// Done`, or `Failed` / `Unavailable` / `Paused`, or the row is removed
/// (cancelled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DownloadStage {
    /// Waiting for a free download slot.
    Queued,
    /// Asking Apple for the playback / asset information (`webPlayback`).
    Requesting,
    /// Negotiating the content license (`widevineCert` +
    /// `acquireWebPlaybackLicense`).
    License,
    /// Fetching the media segments (byte progress refines this band).
    Fetching,
    /// Decrypting / assembling the audio file.
    Decrypting,
    /// Writing the finished file and recording it.
    Saving,
}

impl DownloadStage {
    /// Short, user-facing label ("Requesting", "Downloading", ...).
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Waiting",
            Self::Requesting => "Requesting",
            Self::License => "License",
            Self::Fetching => "Downloading",
            Self::Decrypting => "Decrypting",
            Self::Saving => "Saving",
        }
    }

    /// The `(start, end)` progress band of this stage in `0.0..=1.0`.
    pub fn band(self) -> (f32, f32) {
        match self {
            Self::Queued => (0.0, 0.0),
            Self::Requesting => (0.02, 0.08),
            Self::License => (0.08, 0.15),
            Self::Fetching => (0.15, 0.85),
            Self::Decrypting => (0.85, 0.95),
            Self::Saving => (0.95, 0.99),
        }
    }
}

/// What kind of Apple Music item a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MediaKind {
    /// A song: `webPlayback` returns `assets` with a `ctrp256` flavor.
    #[default]
    Song,
    /// A music video: `webPlayback` returns `assets: []` and an
    /// `hls-playlist-url`; the audio is a separate HLS rendition. Only sent to
    /// a downloader whose [`DownloaderCapabilities::music_videos`] is `true`.
    MusicVideo,
}

/// The playback identity of a request: which ids go into `webPlayback` and
/// the license call, in exactly the shape Cider sends them.
///
/// Core picks the shape; a downloader only serialises it with
/// [`web_playback_body`](Self::web_playback_body),
/// [`is_library`](Self::is_library) and [`adam_id`](Self::adam_id).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlaybackId {
    /// A catalog play (search, album page, every music video):
    /// `{"salableAdamId":"<id>"}` and license `isLibrary: false`.
    Catalog {
        /// The numeric catalog (adam) id.
        salable_adam_id: String,
    },
    /// A play of a song in the user's library that is linked to the catalog
    /// (`playParams.catalogId`):
    /// `{"universalLibraryId":"<i.… or a.…>","subscriptionAdamId":"<id>"}`
    /// and license `isLibrary: true`.
    Library {
        /// The library item id (`i.…` or `a.<catalog id>`), always one whose
        /// resource carries `playParams.catalogId`.
        universal_library_id: String,
        /// The numeric catalog (adam) id it plays as.
        subscription_adam_id: String,
    },
}

impl PlaybackId {
    /// A catalog play of `salable_adam_id`.
    pub fn catalog(salable_adam_id: impl Into<String>) -> Self {
        Self::Catalog {
            salable_adam_id: salable_adam_id.into(),
        }
    }

    /// A library play of `universal_library_id`, which plays as catalog id
    /// `subscription_adam_id`.
    pub fn library(
        universal_library_id: impl Into<String>,
        subscription_adam_id: impl Into<String>,
    ) -> Self {
        Self::Library {
            universal_library_id: universal_library_id.into(),
            subscription_adam_id: subscription_adam_id.into(),
        }
    }

    /// The numeric catalog (adam) id: the license `adamId` and the output
    /// file stem.
    pub fn adam_id(&self) -> &str {
        match self {
            Self::Catalog { salable_adam_id } => salable_adam_id,
            Self::Library {
                subscription_adam_id,
                ..
            } => subscription_adam_id,
        }
    }

    /// The license request's `isLibrary` value.
    pub fn is_library(&self) -> bool {
        matches!(self, Self::Library { .. })
    }

    /// The library item id of a library play.
    pub fn library_id(&self) -> Option<&str> {
        match self {
            Self::Catalog { .. } => None,
            Self::Library {
                universal_library_id,
                ..
            } => Some(universal_library_id),
        }
    }

    /// The exact `POST webPlayback` JSON body Cider sends for this play.
    /// There is never a `features` field.
    pub fn web_playback_body(&self) -> serde_json::Value {
        match self {
            Self::Catalog { salable_adam_id } => {
                serde_json::json!({ "salableAdamId": salable_adam_id })
            }
            Self::Library {
                universal_library_id,
                subscription_adam_id,
            } => serde_json::json!({
                "universalLibraryId": universal_library_id,
                "subscriptionAdamId": subscription_adam_id,
            }),
        }
    }
}

/// What a [`Downloader`] can handle. Core only sends it requests it declared.
///
/// Build it with [`new`](Self::new) and the `with_*` methods; the struct is
/// `#[non_exhaustive]` so capabilities can be added without breaking
/// downloaders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct DownloaderCapabilities {
    /// Whether [`MediaKind::MusicVideo`] requests are supported. While
    /// `false`, the queue marks music videos "Can't download yet" without
    /// calling the downloader.
    pub music_videos: bool,
    /// Whether [`AudioVariant::Spatial`] requests are supported. While
    /// `false`, the queue downloads only the normal version, whatever the
    /// download format says.
    pub spatial: bool,
}

impl DownloaderCapabilities {
    /// Normal songs only.
    pub const fn new() -> Self {
        Self {
            music_videos: false,
            spatial: false,
        }
    }

    /// Declares (or withdraws) music-video support.
    #[must_use]
    pub const fn with_music_videos(mut self, supported: bool) -> Self {
        self.music_videos = supported;
        self
    }

    /// Declares (or withdraws) support for the spatial version of songs.
    #[must_use]
    pub const fn with_spatial(mut self, supported: bool) -> Self {
        self.spatial = supported;
        self
    }
}

/// Track metadata passed to a [`Downloader`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    /// Title.
    pub title: String,
    /// Artist.
    pub artist: String,
    /// Album.
    pub album: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u32>,
}

/// Progress sink handed to a [`Downloader`] through
/// [`DownloadRequest::progress`].
///
/// Call [`stage`](Self::stage) when entering each phase and
/// [`report`](Self::report) as segment bytes arrive. Both are a single
/// unbounded-channel send: cheap, never blocking, never failing. The
/// [`crate::download_queue::DownloadQueue`] drains them, throttles them, and
/// publishes the resulting fraction to the UI ring. As soon as a downloader
/// reports anything, the queue uses its stages (and bytes) instead of the
/// time-based estimate it shows for silent downloaders.
#[derive(Clone)]
pub struct DownloadProgress {
    tx: mpsc::UnboundedSender<ProgressReport>,
}

/// One progress sample from a downloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressReport {
    /// Bytes transferred so far and the total when known.
    Bytes { done: u64, total: Option<u64> },
    /// The download entered a new phase.
    Stage(DownloadStage),
}

impl DownloadProgress {
    /// Builds a sink feeding `tx`. Only the download queue creates these.
    pub(crate) fn new(tx: mpsc::UnboundedSender<ProgressReport>) -> Self {
        Self { tx }
    }

    /// Reports that `done` bytes have been transferred out of `total` (when the
    /// total length is known). Fills the [`DownloadStage::Fetching`] band.
    ///
    /// Never blocks and never fails: if the queue has already stopped listening
    /// (the download finished, was cancelled, or the app is shutting down) the
    /// sample is silently dropped.
    pub fn report(&self, done: u64, total: Option<u64>) {
        let _ = self.tx.send(ProgressReport::Bytes { done, total });
    }

    /// Reports that the download entered `stage`. Same guarantees as
    /// [`report`](Self::report): never blocks, never fails.
    pub fn stage(&self, stage: DownloadStage) {
        let _ = self.tx.send(ProgressReport::Stage(stage));
    }
}

impl std::fmt::Debug for DownloadProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadProgress").finish_non_exhaustive()
    }
}

impl PartialEq for DownloadProgress {
    fn eq(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
}

impl Eq for DownloadProgress {}

/// One request to produce a local audio file for a track.
///
/// Core resolves everything before the call, so a downloader never re-derives
/// ids: [`playback`](Self::playback) is the exact request shape,
/// [`kind`](Self::kind) the media kind, [`output_path`](Self::output_path) the
/// only file it may write, [`progress`](Self::progress) its progress sink and
/// [`cancel`](Self::cancel) its cancellation signal.
///
/// Build it with [`new`](Self::new) and the `with_*` methods (the struct is
/// `#[non_exhaustive]`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DownloadRequest {
    /// Exactly which ids go into `webPlayback` and the license call.
    pub playback: PlaybackId,
    /// Song or music video.
    pub kind: MediaKind,
    /// Which version of the song to produce. Always
    /// [`AudioVariant::Stereo`] unless the downloader declared
    /// [`DownloaderCapabilities::spatial`].
    pub variant: AudioVariant,
    /// The song's `extendedAssetUrls.enhancedHls` master playlist, when the
    /// catalog has one. The queue sets it for [`AudioVariant::Spatial`]
    /// requests: the spatial stream is a variant of this playlist.
    pub enhanced_hls: Option<String>,
    /// The final file to write ([`media_file_name`] in the media directory
    /// for the queue). Write a temporary file next to it first, then rename
    /// it here.
    pub output_path: PathBuf,
    /// Fires when the user pauses or removes the download, or the app shuts
    /// down. Check it between segments and return
    /// [`DownloadError::Cancelled`]. Core also drops the `fetch` future when
    /// it fires, so a check is only needed around blocking / CPU-heavy work.
    pub cancel: CancellationToken,
    /// What is known about the track.
    pub info: TrackInfo,
    /// A user-chosen file, for downloaders that import rather than fetch.
    pub source: Option<PathBuf>,
    /// Progress sink. The download queue always sets it; it is `None` for
    /// direct [`crate::library::Library`] imports.
    pub progress: Option<DownloadProgress>,
}

impl DownloadRequest {
    /// A catalog-play request for the normal version of song `adam_id`,
    /// writing `<media_dir>/<adam_id>.m4a`.
    pub fn new(adam_id: impl Into<String>, media_dir: impl Into<PathBuf>) -> Self {
        let adam_id = adam_id.into();
        Self {
            output_path: media_dir
                .into()
                .join(media_file_name(&adam_id, AudioVariant::Stereo)),
            playback: PlaybackId::catalog(adam_id),
            kind: MediaKind::Song,
            variant: AudioVariant::Stereo,
            enhanced_hls: None,
            cancel: CancellationToken::new(),
            info: TrackInfo::default(),
            source: None,
            progress: None,
        }
    }

    /// Sets the playback identity and renames the output file after its
    /// adam id ([`media_file_name`]).
    #[must_use]
    pub fn with_playback(mut self, playback: PlaybackId) -> Self {
        self.output_path
            .set_file_name(media_file_name(playback.adam_id(), self.variant));
        self.playback = playback;
        self
    }

    /// Sets the variant to produce and renames the output file to match
    /// ([`media_file_name`]).
    #[must_use]
    pub fn with_variant(mut self, variant: AudioVariant) -> Self {
        self.output_path
            .set_file_name(media_file_name(self.playback.adam_id(), variant));
        self.variant = variant;
        self
    }

    /// Sets the song's `enhancedHls` master playlist URL.
    #[must_use]
    pub fn with_enhanced_hls(mut self, url: Option<String>) -> Self {
        self.enhanced_hls = url;
        self
    }

    /// Sets the media kind.
    #[must_use]
    pub fn with_kind(mut self, kind: MediaKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets the track metadata.
    #[must_use]
    pub fn with_info(mut self, info: TrackInfo) -> Self {
        self.info = info;
        self
    }

    /// Sets a user-chosen source file.
    #[must_use]
    pub fn with_source(mut self, source: Option<PathBuf>) -> Self {
        self.source = source;
        self
    }

    /// Sets the progress sink.
    #[must_use]
    pub fn with_progress(mut self, progress: DownloadProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Sets the cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }
}

/// What a successful [`Downloader::fetch`] produced.
///
/// Build it with [`new`](Self::new) and
/// [`with_duration_ms`](Self::with_duration_ms) (the struct is
/// `#[non_exhaustive]`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Downloaded {
    /// The written file (normally [`DownloadRequest::output_path`]).
    pub path: PathBuf,
    /// Which version of the song the file holds.
    pub variant: AudioVariant,
    /// Playback length of the file in milliseconds, when the downloader
    /// knows it.
    pub duration_ms: Option<u32>,
}

impl Downloaded {
    /// A `variant` file at `path` with no known duration.
    pub fn new(path: impl Into<PathBuf>, variant: AudioVariant) -> Self {
        Self {
            path: path.into(),
            variant,
            duration_ms: None,
        }
    }

    /// Sets the file's playback length.
    #[must_use]
    pub fn with_duration_ms(mut self, duration_ms: Option<u32>) -> Self {
        self.duration_ms = duration_ms;
        self
    }
}

/// Produces a local audio file for a track.
///
/// The contract between core and a downloader:
///
/// - **Request**: [`DownloadRequest::playback`] says exactly which ids go into
///   `webPlayback` and the license call, and [`DownloadRequest::kind`] whether
///   it is a song or a music video. Core has already resolved both, including
///   the playable twin of a library-only item. [`DownloadRequest::variant`]
///   names the version to produce.
/// - **Output**: write a temporary file, then rename it to
///   [`DownloadRequest::output_path`]. Write nowhere else. Return
///   [`Downloaded`] with the variant actually produced (and the duration,
///   when known), or [`DownloadError::VariantUnavailable`] when the song has
///   no such version.
/// - **Errors**: return the typed [`DownloadError`] variant that fits
///   ([`DownloadError::Unavailable`], [`DownloadError::Transient`],
///   [`DownloadError::AuthExpired`], ...). Core maps each one to retry, no retry or
///   re-sign-in; the downloader itself never retries or sleeps.
/// - **Progress and cancellation**: report progress to
///   [`DownloadRequest::progress`] when it is set, and check [`DownloadRequest::cancel`] between segments
///   and around blocking work.
/// - **Capabilities**: declare what it supports with
///   [`Downloader::capabilities`]; music videos and spatial requests are only
///   sent to a downloader that declares them.
#[async_trait]
pub trait Downloader: Send + Sync + 'static {
    /// Stable registry id.
    fn id(&self) -> &'static str;

    /// What this downloader supports. Defaults to songs only.
    fn capabilities(&self) -> DownloaderCapabilities {
        DownloaderCapabilities::new()
    }

    /// Produces the audio file and says what it wrote.
    async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError>;
}

/// Downloaders by id. Always contains [`LocalImportDownloader`].
#[derive(Clone)]
pub struct DownloaderRegistry {
    downloaders: HashMap<&'static str, Arc<dyn Downloader>>,
}

impl std::fmt::Debug for DownloaderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloaderRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl Default for DownloaderRegistry {
    fn default() -> Self {
        let mut registry = Self {
            downloaders: HashMap::new(),
        };
        registry.register(LocalImportDownloader);
        registry
    }
}

impl DownloaderRegistry {
    /// Registry with only [`LocalImportDownloader`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces, by id) a downloader.
    pub fn register(&mut self, downloader: impl Downloader) {
        self.register_arc(Arc::new(downloader));
    }

    /// Adds (or replaces, by id) a shared downloader.
    pub fn register_arc(&mut self, downloader: Arc<dyn Downloader>) {
        self.downloaders.insert(downloader.id(), downloader);
    }

    /// Looks up a downloader.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Downloader>> {
        self.downloaders.get(id).cloned()
    }

    /// Registered ids, sorted.
    pub fn ids(&self) -> Vec<&'static str> {
        let mut ids: Vec<_> = self.downloaders.keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

/// Copies a user-chosen local audio file into the media directory as
/// `<track id>.<ext>` (atomically: temp file + rename).
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalImportDownloader;

/// Lower-cased audio extension of `path`, if supported.
pub(crate) fn audio_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    AUDIO_EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

#[async_trait]
impl Downloader for LocalImportDownloader {
    fn id(&self) -> &'static str {
        LOCAL_IMPORT
    }

    async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
        let source = request
            .source
            .as_deref()
            .ok_or(DownloadError::SourceRequired)?;
        let ext = audio_extension(source)
            .ok_or_else(|| DownloadError::UnsupportedFormat(source.to_path_buf()))?;
        let id = request.playback.adam_id();
        if !is_valid_media_id(id) {
            return Err(DownloadError::InvalidTrackId(id.to_owned()));
        }
        let media_dir = request.output_path.parent().unwrap_or(Path::new(""));
        tokio::fs::create_dir_all(media_dir).await?;
        let dest = media_dir.join(format!("{id}.{ext}"));
        let tmp = media_dir.join(format!("{id}.{ext}.part"));
        tokio::fs::copy(source, &tmp).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        // An imported file is always the song's normal version.
        Ok(Downloaded::new(dest, AudioVariant::Stereo))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(dir: &Path, source: Option<PathBuf>) -> DownloadRequest {
        DownloadRequest::new("123", dir.join("media")).with_source(source)
    }

    #[tokio::test]
    async fn import_should_copy_into_media_dir() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Song.MP3");
        std::fs::write(&source, b"abc").unwrap();
        let dest = LocalImportDownloader
            .fetch(&request(dir.path(), Some(source)))
            .await
            .unwrap();
        assert_eq!(dest.path, dir.path().join("media").join("123.mp3"));
    }

    #[tokio::test]
    async fn import_without_source_should_fail() {
        let dir = tempfile::tempdir().unwrap();
        let error = LocalImportDownloader
            .fetch(&request(dir.path(), None))
            .await
            .unwrap_err();
        assert!(matches!(error, DownloadError::SourceRequired));
    }

    #[tokio::test]
    async fn import_should_reject_non_audio() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("notes.txt");
        std::fs::write(&source, b"abc").unwrap();
        let error = LocalImportDownloader
            .fetch(&request(dir.path(), Some(source)))
            .await
            .unwrap_err();
        assert!(matches!(error, DownloadError::UnsupportedFormat(_)));
    }

    #[test]
    fn catalog_play_should_send_salable_adam_id_only() {
        let body = PlaybackId::catalog("1443063973").web_playback_body();
        assert_eq!(body, serde_json::json!({ "salableAdamId": "1443063973" }));
    }

    #[test]
    fn library_play_should_send_library_and_subscription_ids() {
        let body = PlaybackId::library("a.6814096109", "6814096109").web_playback_body();
        assert_eq!(
            body,
            serde_json::json!({
                "universalLibraryId": "a.6814096109",
                "subscriptionAdamId": "6814096109",
            })
        );
    }

    #[test]
    fn library_play_should_set_is_library() {
        assert!(PlaybackId::library("i.x", "1").is_library());
    }

    #[test]
    fn with_playback_should_name_output_after_adam_id() {
        let request = DownloadRequest::new("0", "/media")
            .with_playback(PlaybackId::library("a.6814096109", "6814096109"));
        assert_eq!(
            request.output_path,
            PathBuf::from("/media").join("6814096109.m4a")
        );
    }

    #[test]
    fn default_capabilities_should_exclude_music_videos() {
        struct Plain;
        #[async_trait]
        impl Downloader for Plain {
            fn id(&self) -> &'static str {
                "plain"
            }
            async fn fetch(&self, _: &DownloadRequest) -> Result<Downloaded, DownloadError> {
                Err(DownloadError::Cancelled)
            }
        }
        assert!(!Plain.capabilities().music_videos);
    }

    #[test]
    fn default_capabilities_should_exclude_spatial() {
        assert!(!DownloaderCapabilities::default().spatial);
    }

    #[test]
    fn with_variant_should_name_spatial_output_after_adam_id() {
        let request = DownloadRequest::new("0", "/media")
            .with_playback(PlaybackId::library("a.6814096109", "6814096109"))
            .with_variant(AudioVariant::Spatial);
        assert_eq!(
            request.output_path,
            PathBuf::from("/media").join("6814096109.spatial.m4a")
        );
    }

    #[test]
    fn with_playback_should_keep_the_spatial_file_name() {
        let request = DownloadRequest::new("0", "/media")
            .with_variant(AudioVariant::Spatial)
            .with_playback(PlaybackId::catalog("42"));
        assert_eq!(
            request.output_path,
            PathBuf::from("/media").join("42.spatial.m4a")
        );
    }

    #[test]
    fn new_request_should_ask_for_the_normal_variant() {
        assert_eq!(
            DownloadRequest::new("1", "/m").variant,
            AudioVariant::Stereo
        );
    }

    #[test]
    fn registry_should_contain_local_import() {
        assert_eq!(DownloaderRegistry::new().ids(), [LOCAL_IMPORT]);
    }
}
