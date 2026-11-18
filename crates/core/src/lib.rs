//! Proxy engine for Cider Offline.
//!
//! The entry point is [`ProxyHandle`]: start it with a [`Config`], switch
//! [`Mode`] / capture at runtime, observe [`ProxyStatus`] through
//! [`ProxyHandle::subscribe`], and stop it with [`ProxyHandle::shutdown`].
//!
//! Request processing for Apple Music hosts in [`Mode::Active`] is an ordered
//! pipeline of [`Interceptor`]s. Built in, in order: token capture,
//! [`LibraryInjectionInterceptor`], [`ResponseCacheInterceptor`],
//! [`lyrics::LyricsInterceptor`], [`LocalPlaybackInterceptor`], then any
//! added with [`ProxyHandleBuilder::interceptor`], then [`OfflineResponder`].
//!
//! The UI manages local audio, lyrics and the cache through [`Library`]
//! ([`ProxyHandle::library`]).

mod apple;
pub mod artwork;
mod cache;
pub mod catalog;
pub mod config;
pub mod connectivity;
pub mod download;
pub mod download_queue;
mod download_target;
mod inject;
pub mod library;
pub mod library_sync;
pub mod lyrics;
pub mod media;
mod mode;
mod offline;
mod playback;
pub mod proxy;
mod respond;
pub mod storage;

/// Imports shared by the built-in interceptors.
mod interceptor_prelude {
    pub(crate) use crate::proxy::{Exchange, Interceptor, RequestOutcome};
    pub(crate) use async_trait::async_trait;
    pub(crate) use hudsucker::hyper::Response;
}

pub use cache::ResponseCacheInterceptor;
pub use catalog::{Album, CatalogClient, CatalogError, CatalogItem, Playlist, Song, SongCandidate};
pub use config::{
    AppPaths, AutoCacheMode, Config, ConfigError, DetailLevel, LibrarySettings, SongsPage,
};
pub use download::{
    DownloadError, DownloadProgress, DownloadStage, DownloaderCapabilities, MediaKind, PlaybackId,
};
pub use download_queue::{
    DEFAULT_CONCURRENCY, DownloadEvent, DownloadProgressUpdate, DownloadQueue, LyricsFetcher,
    MAX_CONCURRENCY, QueueError, clamp_concurrency, enqueue_missing_reference,
    records_from_catalog,
};
pub use download_target::{AlbumVersion, album_version};
pub use inject::LibraryInjectionInterceptor;
pub use library::{
    ArtworkRequest, Library, LibraryError, LyricsProgress, LyricsReport, PrefetchReport,
    ResolvedArtwork, identity_key,
};
pub use library_sync::{LibrarySync, SyncError, SyncStatus};
pub use lyrics::{FailurePolicy, LyricsProviderId, LyricsQuality, LyricsSettings};
pub use mode::Mode;
pub use offline::OfflineResponder;
pub use playback::LocalPlaybackInterceptor;
pub use proxy::{
    Counters, Exchange, Interceptor, ProxyError, ProxyHandle, ProxyHandleBuilder, ProxyStatus,
    RequestOutcome,
};
pub use storage::{
    DownloadRecord, DownloadSourceKind, DownloadStatus, LibraryCollection, LibraryContents,
    LibraryFilter, LibraryPage, LibraryQuery, LibrarySnapshot, LibraryTrack, Storage, StorageError,
    SyncedCollection, TrackFilter,
};

/// Re-exports needed to implement an [`Interceptor`].
pub use async_trait::async_trait;
pub use hudsucker::{Body, hyper};
/// Cancellation handle taken by long-running library operations.
pub use tokio_util::sync::CancellationToken;
