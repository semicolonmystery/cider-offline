//! The MITM proxy and its public control handle.

pub mod body;
mod cache_key;
mod capture;
mod handler;
pub mod hosts;
mod interceptor;
mod status;
mod tokens;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use cider_offline_trust::{CertificateAuthority, TrustError};
use hudsucker::Proxy;
use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::rustls::crypto::aws_lc_rs;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;

pub use cache_key::{VOLATILE_QUERY_PARAMS, cache_key, normalize_query};
pub use interceptor::{Exchange, Interceptor, RequestOutcome};
pub use status::{Counters, ProxyStatus};
pub use tokens::TokenCaptureInterceptor;

use self::capture::CaptureWriter;
use self::handler::{ProxyHttpHandler, Shared};
use self::interceptor::Pipeline;
use crate::Mode;
use crate::artwork::ArtworkStore;
use crate::cache::ResponseCacheInterceptor;
use crate::catalog::CatalogClient;
use crate::config::{AppPaths, AutoCacheMode, Config, ConfigError, SongsPage};
use crate::connectivity::{ConnectivityError, ConnectivityMonitor};
use crate::download::{Downloader, DownloaderRegistry};
use crate::download_queue::DownloadQueue;
use crate::inject::LibraryInjectionInterceptor;
use crate::library::{Library, LibraryError};
use crate::library_sync::LibrarySync;
use crate::lyrics::{LyricsFlagsInterceptor, LyricsInterceptor, LyricsStore};
use crate::media::MediaResolver;
use crate::offline::OfflineResponder;
use crate::playback::{AutoCache, LocalPlaybackInterceptor};
use crate::storage::{Storage, StorageError};

/// Number of per-host leaf certificates kept in memory.
const LEAF_CERT_CACHE_SIZE: u64 = 1_000;
/// How long [`ProxyHandle::shutdown`] waits for connections and background
/// tasks to drain before giving up on them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Errors starting or stopping the proxy.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Locating or creating app directories failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Loading or creating the local CA failed.
    #[error(transparent)]
    Trust(#[from] TrustError),
    /// Opening the local store failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The listening socket could not be bound.
    #[error("could not listen on {addr}: {source}")]
    Bind {
        /// Requested address.
        addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The connectivity monitor could not be created.
    #[error(transparent)]
    Connectivity(#[from] ConnectivityError),
    /// The proxy server failed.
    #[error("proxy server error: {0}")]
    Server(#[from] hudsucker::Error),
    /// A background task panicked.
    #[error("proxy task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The library facade could not be created.
    #[error(transparent)]
    Library(#[from] LibraryError),
    /// The library sync could not be created.
    #[error(transparent)]
    LibrarySync(#[from] crate::library_sync::SyncError),
}

/// Configures and starts a [`ProxyHandle`].
pub struct ProxyHandleBuilder {
    config: Config,
    paths: Option<AppPaths>,
    storage: Option<Storage>,
    interceptors: Vec<Arc<dyn Interceptor>>,
    downloaders: DownloaderRegistry,
}

impl ProxyHandleBuilder {
    fn new(config: Config) -> Self {
        Self {
            config,
            paths: None,
            storage: None,
            interceptors: Vec::new(),
            downloaders: DownloaderRegistry::new(),
        }
    }

    /// Registers an additional [`Downloader`] for [`Library::download`]
    /// (see [`crate::download`]).
    #[must_use]
    pub fn downloader(mut self, downloader: impl Downloader) -> Self {
        self.downloaders.register(downloader);
        self
    }

    /// Reuses an already-opened [`Storage`] handle instead of opening a new
    /// one inside [`start`](Self::start).
    #[must_use]
    pub fn storage(mut self, storage: Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Uses explicit directories instead of [`AppPaths::discover`].
    #[must_use]
    pub fn paths(mut self, paths: AppPaths) -> Self {
        self.paths = Some(paths);
        self
    }

    /// Appends an interceptor. Interceptors run in the order added, after the
    /// built-in token capture, library injection, response cache, lyrics and
    /// local playback interceptors, and before the built-in offline responder
    /// (which answers everything left over while offline).
    #[must_use]
    pub fn interceptor(mut self, interceptor: impl Interceptor) -> Self {
        self.interceptors.push(Arc::new(interceptor));
        self
    }

    /// Appends a shared interceptor (see [`Self::interceptor`]).
    #[must_use]
    pub fn interceptor_arc(mut self, interceptor: Arc<dyn Interceptor>) -> Self {
        self.interceptors.push(interceptor);
        self
    }

    /// Prepares directories, CA and storage, binds the listener and starts
    /// the proxy and connectivity monitor on the current Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if any of those steps fails (e.g. port in use).
    #[allow(clippy::too_many_lines)] // linear startup sequence; splitting hurts readability
    pub async fn start(self) -> Result<ProxyHandle, ProxyError> {
        let paths = match self.paths {
            Some(paths) => paths,
            None => AppPaths::discover()?,
        };
        let config = self.config;

        let (paths, ca) = tokio::task::spawn_blocking(move || {
            paths.ensure_dirs()?;
            let ca = CertificateAuthority::load_or_create(&paths.ca_dir())?;
            Ok::<_, ProxyError>((paths, ca))
        })
        .await??;
        let storage = match self.storage {
            Some(storage) => storage,
            None => Storage::open_async(paths.database_file()).await?,
        };
        let authority = RcgenAuthority::new(
            ca.issuer()?,
            LEAF_CERT_CACHE_SIZE,
            aws_lc_rs::default_provider(),
        );

        let addr = config.listen_addr();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ProxyError::Bind { addr, source })?;
        let listening = listener
            .local_addr()
            .map_err(|source| ProxyError::Bind { addr, source })?;

        let connectivity = Arc::new(Connectivity {
            forced_offline: AtomicBool::new(config.connectivity.force_offline),
            probe_online: AtomicBool::new(true),
        });
        let status = watch::Sender::new(ProxyStatus {
            mode: config.mode,
            capture: config.capture,
            online: !config.connectivity.force_offline,
            running: true,
            listening,
            counters: Counters::default(),
            last_error: None,
        });
        let tasks = TaskTracker::new();
        let cancel = CancellationToken::new();

        let downloaders = Arc::new(self.downloaders);
        let library = Library::with_options_arc(
            storage.clone(),
            paths.clone(),
            crate::lyrics::LrclibClient::new(crate::lyrics::LRCLIB_BASE_URL)
                .map_err(LibraryError::from)?,
            Arc::clone(&downloaders),
        )?;
        library.apply_lyrics_config(&config);
        let library_sync = LibrarySync::with_artwork_prefetch(storage.clone(), library.clone())?;
        let last_played_ts = Arc::new(AtomicU64::new(0));
        let downloads = DownloadQueue::spawn(
            storage.clone(),
            Arc::clone(&downloaders),
            paths.media_dir(),
            Arc::clone(&last_played_ts),
            // Every completed download (manual or auto-cache) also gets lyrics.
            Some(Arc::new(library.clone())),
        );
        downloads.set_concurrency(config.download_concurrency());
        let live = LiveSettings {
            auto_cache: Arc::new(AtomicU8::new(config.auto_cache.as_u8())),
            songs_page: Arc::new(AtomicU8::new(config.library.songs_page.as_u8())),
            prefer_local_lyrics: Arc::new(AtomicBool::new(config.lyrics.always_use_downloaded)),
        };
        let lyrics = library.lyrics_store();
        {
            let lyrics = lyrics.clone();
            tasks.spawn(async move {
                let count = lyrics.load_index().await;
                tracing::debug!(count, "indexed local lyrics");
            });
        }
        let interceptors = builtin_pipeline(
            &storage,
            &paths,
            lyrics,
            downloads.clone(),
            &live,
            Arc::clone(&last_played_ts),
            library_sync.clone(),
            self.interceptors,
        );
        let shared = Arc::new(Shared::new(
            status.clone(),
            Pipeline::new(interceptors),
            storage.clone(),
            CaptureWriter::new(paths.captures_dir(), tasks.clone()),
            tasks.clone(),
        ));

        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(ProxyHttpHandler::new(Arc::clone(&shared)))
            .with_graceful_shutdown(cancel.clone().cancelled_owned())
            .build()?;

        let server_status = status.clone();
        let server = tokio::spawn(async move {
            let result = proxy.start().await;
            server_status.send_modify(|s| {
                s.running = false;
                if let Err(error) = &result {
                    s.last_error = Some(format!("proxy server stopped: {error}"));
                }
            });
            result
        });

        if config.connectivity.enabled {
            let monitor = ConnectivityMonitor::new(&config.connectivity)?;
            let online_status = status.clone();
            let connectivity = Arc::clone(&connectivity);
            tasks.spawn(monitor.run(cancel.clone(), move |online| {
                connectivity.probe_online.store(online, Ordering::Relaxed);
                connectivity.publish(&online_status);
            }));
        }

        tasks.spawn(library_sync.clone().run(cancel.clone(), status.subscribe()));

        tracing::info!(%listening, mode = %config.mode, "proxy started");
        Ok(ProxyHandle {
            shared,
            connectivity,
            ca: Arc::new(ca),
            library,
            library_sync,
            downloads,
            live,
            paths,
            server,
            tasks,
            cancel: cancel.drop_guard(),
        })
    }
}

/// Settings the running pipeline reads on every request, so changing them
/// needs no restart.
struct LiveSettings {
    /// [`AutoCacheMode::as_u8`].
    auto_cache: Arc<AtomicU8>,
    /// [`SongsPage::as_u8`].
    songs_page: Arc<AtomicU8>,
    /// [`crate::lyrics::LyricsSettings::always_use_downloaded`].
    prefer_local_lyrics: Arc<AtomicBool>,
}

/// The built-in interceptors around the user-supplied `extra` ones.
///
/// Request hooks run top to bottom, response hooks bottom to top: the cache
/// stores upstream bodies before library injection modifies them, and the
/// offline responder answers whatever is left while offline (and the library
/// Songs listing while online, when it is set to list every song).
#[expect(
    clippy::too_many_arguments,
    reason = "one call site; a struct would only rename the arguments"
)]
fn builtin_pipeline(
    storage: &Storage,
    paths: &AppPaths,
    lyrics: LyricsStore,
    downloads: DownloadQueue,
    live: &LiveSettings,
    last_played_ts: Arc<AtomicU64>,
    library_sync: LibrarySync,
    extra: Vec<Arc<dyn Interceptor>>,
) -> Vec<Arc<dyn Interceptor>> {
    let artwork = ArtworkStore::new(paths.artwork_dir(), storage.clone());
    let auto = AutoCache::new(
        Arc::clone(&live.auto_cache),
        downloads.clone(),
        CatalogClient::new(storage.clone()),
        last_played_ts,
    );
    let playback = LocalPlaybackInterceptor::new(MediaResolver::new(storage.clone()))
        .with_auto_cache(auto)
        .with_queue(downloads);
    let mut interceptors: Vec<Arc<dyn Interceptor>> = vec![
        // First, so its response hook runs last: it annotates every song
        // resource Cider gets (upstream, injected or offline) with our lyrics.
        Arc::new(LyricsFlagsInterceptor::new(lyrics.clone())),
        Arc::new(TokenCaptureInterceptor::new().with_library_sync(library_sync)),
        Arc::new(LibraryInjectionInterceptor),
        Arc::new(ResponseCacheInterceptor::new(artwork.clone())),
        Arc::new(LyricsInterceptor::new(
            lyrics,
            Arc::clone(&live.prefer_local_lyrics),
        )),
        Arc::new(playback),
    ];
    interceptors.extend(extra);
    interceptors.push(Arc::new(OfflineResponder::new(
        artwork,
        Arc::clone(&live.songs_page),
    )));
    interceptors
}

/// Effective connectivity: the last probe result, overridden by the
/// forced-offline switch.
struct Connectivity {
    forced_offline: AtomicBool,
    probe_online: AtomicBool,
}

impl Connectivity {
    fn effective(&self) -> bool {
        !self.forced_offline.load(Ordering::Relaxed) && self.probe_online.load(Ordering::Relaxed)
    }

    /// Writes the effective state into the status (notifying only on change).
    fn publish(&self, status: &watch::Sender<ProxyStatus>) {
        let online = self.effective();
        status.send_if_modified(|s| {
            let changed = s.online != online;
            s.online = online;
            changed
        });
    }
}

/// Control handle of a running proxy.
///
/// Dropping the handle without calling [`ProxyHandle::shutdown`] still
/// signals the proxy to stop, but does not wait for it.
pub struct ProxyHandle {
    shared: Arc<Shared>,
    connectivity: Arc<Connectivity>,
    ca: Arc<CertificateAuthority>,
    library: Library,
    library_sync: LibrarySync,
    downloads: DownloadQueue,
    live: LiveSettings,
    paths: AppPaths,
    server: JoinHandle<Result<(), hudsucker::Error>>,
    tasks: TaskTracker,
    cancel: DropGuard,
}

impl std::fmt::Debug for ProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHandle")
            .field("status", &*self.shared.status.borrow())
            .finish_non_exhaustive()
    }
}

impl ProxyHandle {
    /// Starts a proxy with the default directories and built-in interceptors.
    ///
    /// # Errors
    ///
    /// See [`ProxyHandleBuilder::start`].
    pub async fn start(config: Config) -> Result<Self, ProxyError> {
        Self::builder(config).start().await
    }

    /// Returns a builder for custom paths or extra interceptors.
    pub fn builder(config: Config) -> ProxyHandleBuilder {
        ProxyHandleBuilder::new(config)
    }

    /// Switches the mode. Takes effect for new requests and new CONNECT
    /// tunnels immediately; already-established raw tunnels stay raw until
    /// the client reconnects.
    pub fn set_mode(&self, mode: Mode) {
        let changed = self.shared.status.send_if_modified(|s| {
            let changed = s.mode != mode;
            s.mode = mode;
            changed
        });
        if changed {
            tracing::info!(%mode, "mode changed");
        }
    }

    /// Forces offline mode on or off. While forced, every intercepted request
    /// is answered locally by the offline responder, whatever the probe says;
    /// switching it off restores the last probe result.
    pub fn set_force_offline(&self, forced: bool) {
        let previous = self
            .connectivity
            .forced_offline
            .swap(forced, Ordering::Relaxed);
        self.connectivity.publish(&self.shared.status);
        if previous != forced {
            tracing::info!(forced, "forced offline changed");
        }
    }

    /// Enables or disables capture-to-disk for new requests.
    pub fn set_capture(&self, capture: bool) {
        let changed = self.shared.status.send_if_modified(|s| {
            let changed = s.capture != capture;
            s.capture = capture;
            changed
        });
        if changed {
            tracing::info!(capture, "capture changed");
        }
    }

    /// Switches the auto-cache mode. Takes effect for subsequent playback
    /// requests.
    pub fn set_auto_cache(&self, mode: AutoCacheMode) {
        let previous = self.live.auto_cache.swap(mode.as_u8(), Ordering::Relaxed);
        if previous != mode.as_u8() {
            tracing::info!(auto_cache = ?mode, "auto-cache changed");
        }
    }

    /// Turns "Always use downloaded lyrics" on or off. Takes effect for the
    /// next lyrics request.
    pub fn set_prefer_local_lyrics(&self, enabled: bool) {
        let previous = self
            .live
            .prefer_local_lyrics
            .swap(enabled, Ordering::Relaxed);
        if previous != enabled {
            tracing::info!(enabled, "always use downloaded lyrics changed");
        }
    }

    /// Switches which songs Cider's library Songs page lists. Takes effect
    /// for the next request, online and offline.
    pub fn set_songs_page(&self, page: SongsPage) {
        let previous = self.live.songs_page.swap(page.as_u8(), Ordering::Relaxed);
        if previous != page.as_u8() {
            tracing::info!(songs_page = ?page, "songs page changed");
        }
    }

    /// Subscribes to status updates.
    pub fn subscribe(&self) -> watch::Receiver<ProxyStatus> {
        self.shared.status.subscribe()
    }

    /// Current status snapshot.
    pub fn status(&self) -> ProxyStatus {
        self.shared.status.borrow().clone()
    }

    /// Address the proxy listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.status.borrow().listening
    }

    /// The local store used by the proxy.
    pub fn storage(&self) -> &Storage {
        &self.shared.storage
    }

    /// The local CA (for trust-store installation and export).
    pub fn certificate_authority(&self) -> &Arc<CertificateAuthority> {
        &self.ca
    }

    /// Directories in use.
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// Library facade (import audio, lyrics, removal, cache, artwork) over
    /// this proxy's store and directories. Cheap to clone; stays usable
    /// after shutdown.
    pub fn library(&self) -> Library {
        self.library.clone()
    }

    /// Active Apple Music library sync (request a sync, observe its status).
    pub fn library_sync(&self) -> &LibrarySync {
        &self.library_sync
    }

    /// Download queue driving the registered downloader for pre-resolved
    /// tracks. Cheap to clone; events are broadcast from one actor.
    pub fn downloads(&self) -> &DownloadQueue {
        &self.downloads
    }

    /// Stops accepting connections, drains in-flight ones and background
    /// tasks (bounded by a grace period), and waits for the server to exit.
    ///
    /// # Errors
    ///
    /// Returns the server's error if it failed, or a task error if it panicked.
    pub async fn shutdown(self) -> Result<(), ProxyError> {
        let Self {
            shared,
            mut server,
            tasks,
            cancel,
            ..
        } = self;
        drop(cancel); // cancels the token

        let result = if let Ok(joined) = tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
            joined?.map_err(ProxyError::from)
        } else {
            tracing::warn!("proxy did not drain in time; aborting open connections");
            server.abort();
            Ok(())
        };

        tasks.close();
        if tokio::time::timeout(SHUTDOWN_GRACE, tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("background tasks did not finish in time");
        }
        shared.status.send_modify(|s| s.running = false);
        tracing::info!("proxy stopped");
        result
    }
}

#[cfg(test)]
mod tests;
