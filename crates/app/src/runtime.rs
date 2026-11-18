//! The tokio side of the app: a small actor that owns the [`ProxyHandle`].
//!
//! The UI never touches the proxy directly. It sends [`Command`]s through a
//! [`ProxyService`] and receives [`ServiceEvent`]s through a sink closure.
//! The actor also publishes a [`Backend`] (store, library, CA, paths) through
//! a `watch` channel so UI tasks can run storage queries on the runtime.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use cider_offline_core::{
    AppPaths, AutoCacheMode, Config, DownloadEvent, DownloadQueue, Library, LibrarySync, Mode,
    ProxyHandle, ProxyStatus, SongsPage, Storage, SyncStatus,
};
use cider_offline_download::DrmDownloader;
use cider_offline_trust::CertificateAuthority;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// Minimum spacing of status events forwarded to the UI. The proxy updates
/// its counters on every request, which would otherwise flood the UI thread.
const STATUS_THROTTLE: Duration = Duration::from_millis(250);
/// Minimum spacing of per-row download status events forwarded to the UI.
const DOWNLOAD_STATUS_THROTTLE: Duration = Duration::from_millis(250);

/// Services of a running proxy, cloned into UI tasks.
#[derive(Clone)]
pub struct Backend {
    /// The local store.
    pub storage: Storage,
    /// Library facade (audio import, lyrics, removal, cache, artwork).
    pub library: Library,
    /// The proxy's root CA.
    pub ca: Arc<CertificateAuthority>,
    /// Download queue (enqueue / retry / cancel / clear finished).
    pub downloads: DownloadQueue,
    /// Active Apple Music library sync (Refresh requests a run).
    pub library_sync: LibrarySync,
}

/// Events from the proxy actor to the UI.
#[derive(Debug, Clone)]
pub enum ServiceEvent {
    /// The proxy is (re)starting.
    Starting,
    /// The proxy started; `restarted` is false for the first start.
    Started { restarted: bool },
    /// Latest status snapshot (throttled).
    Status(ProxyStatus),
    /// A download-queue event (per-row updates are throttled).
    Download(DownloadEvent),
    /// A library sync run finished (successfully or not).
    LibrarySync(SyncStatus),
    /// Download events were dropped (the UI fell behind); re-read the rows.
    DownloadsResync,
    /// Starting the proxy failed.
    Failed(String),
}

enum Command {
    SetMode(Mode),
    SetCapture(bool),
    SetAutoCacheMode(AutoCacheMode),
    SetSongsPage(SongsPage),
    SetPreferLocalLyrics(bool),
    Restart(Box<Config>),
    Shutdown(oneshot::Sender<()>),
}

/// Handle used by the UI to control the proxy actor. Cheap to clone.
#[derive(Clone)]
pub struct ProxyService {
    commands: mpsc::UnboundedSender<Command>,
    backend: watch::Receiver<Option<Backend>>,
}

impl ProxyService {
    /// Spawns the actor on `rt`; it starts the proxy immediately.
    pub fn spawn(
        rt: &Handle,
        config: Config,
        paths: AppPaths,
        sink: impl Fn(ServiceEvent) + Send + 'static,
    ) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        let (backend_tx, backend) = watch::channel(None);
        let actor = Actor {
            config,
            paths,
            proxy: None,
            backend: backend_tx,
            sink: Box::new(sink),
        };
        rt.spawn(actor.run(rx));
        Self { commands, backend }
    }

    /// Switches the proxy mode.
    pub fn set_mode(&self, mode: Mode) {
        self.send(Command::SetMode(mode));
    }

    /// Enables or disables capture-to-disk.
    pub fn set_capture(&self, capture: bool) {
        self.send(Command::SetCapture(capture));
    }

    /// Switches the auto-cache mode for played songs.
    pub fn set_auto_cache_mode(&self, mode: AutoCacheMode) {
        self.send(Command::SetAutoCacheMode(mode));
    }

    /// Switches which songs Cider's library Songs page lists.
    pub fn set_songs_page(&self, page: SongsPage) {
        self.send(Command::SetSongsPage(page));
    }

    /// Turns "Always use downloaded lyrics" on or off (no restart).
    pub fn set_prefer_local_lyrics(&self, enabled: bool) {
        self.send(Command::SetPreferLocalLyrics(enabled));
    }

    /// Restarts the proxy with a new configuration (e.g. a new port).
    pub fn restart(&self, config: Config) {
        self.send(Command::Restart(Box::new(config)));
    }

    /// Stops the proxy gracefully and ends the actor.
    pub async fn shutdown(&self) {
        let (done, wait) = oneshot::channel();
        if self.commands.send(Command::Shutdown(done)).is_ok() {
            // An error means the actor is already gone, which is fine.
            let _ = wait.await;
        }
    }

    /// Waits until the proxy has started once and returns its services, or
    /// `None` if the actor ended first.
    pub async fn wait_backend(&self) -> Option<Backend> {
        let mut rx = self.backend.clone();
        rx.wait_for(Option::is_some).await.ok()?.clone()
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::warn!("proxy actor is not running; command dropped");
        }
    }
}

struct Actor {
    config: Config,
    paths: AppPaths,
    proxy: Option<ProxyHandle>,
    backend: watch::Sender<Option<Backend>>,
    sink: Box<dyn Fn(ServiceEvent) + Send>,
}

impl Actor {
    /// Applies a setting the running proxy takes without a restart.
    fn apply_live(&mut self, command: &Command) {
        match *command {
            Command::SetMode(mode) => {
                self.config.mode = mode;
                if let Some(proxy) = &self.proxy {
                    proxy.set_mode(mode);
                }
            }
            Command::SetCapture(capture) => {
                self.config.capture = capture;
                if let Some(proxy) = &self.proxy {
                    proxy.set_capture(capture);
                }
            }
            Command::SetAutoCacheMode(mode) => {
                self.config.auto_cache = mode;
                if let Some(proxy) = &self.proxy {
                    proxy.set_auto_cache(mode);
                }
            }
            Command::SetSongsPage(page) => {
                self.config.library.songs_page = page;
                if let Some(proxy) = &self.proxy {
                    proxy.set_songs_page(page);
                }
            }
            Command::SetPreferLocalLyrics(enabled) => {
                self.config.lyrics.always_use_downloaded = enabled;
                if let Some(proxy) = &self.proxy {
                    proxy.set_prefer_local_lyrics(enabled);
                }
            }
            Command::Restart(_) | Command::Shutdown(_) => {}
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.start(false).await;
        let mut status = self.proxy.as_ref().map(ProxyHandle::subscribe);
        let mut downloads = self.proxy.as_ref().map(|p| p.downloads().events());
        let mut library_sync = self.proxy.as_ref().map(|p| p.library_sync().subscribe());
        let mut throttle = tokio::time::interval(STATUS_THROTTLE);
        throttle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut download_throttle = tokio::time::interval(DOWNLOAD_STATUS_THROTTLE);
        download_throttle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = false;
        let mut pending_status: HashMap<String, DownloadEvent> = HashMap::new();

        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        Command::Restart(config) => {
                            self.restart(*config).await;
                            status = self.proxy.as_ref().map(ProxyHandle::subscribe);
                            downloads = self.proxy.as_ref().map(|p| p.downloads().events());
                            library_sync =
                                self.proxy.as_ref().map(|p| p.library_sync().subscribe());
                            dirty = true;
                        }
                        Command::Shutdown(done) => {
                            self.shut_down().await;
                            let _ = done.send(());
                            return;
                        }
                        live => self.apply_live(&live),
                    }
                }
                changed = changed(status.as_mut()) => {
                    if changed {
                        dirty = true;
                    } else {
                        status = None;
                    }
                }
                finished = sync_finished(library_sync.as_mut()) => match finished {
                    Some(status) => (self.sink)(ServiceEvent::LibrarySync(status)),
                    None => library_sync = None,
                },
                _ = throttle.tick(), if dirty => {
                    dirty = false;
                    if let Some(rx) = status.as_mut() {
                        let snapshot = rx.borrow_and_update().clone();
                        (self.sink)(ServiceEvent::Status(snapshot));
                    }
                }
                evt = recv_download(downloads.as_mut()) => {
                    match evt {
                        DownloadRecvOutcome::Event(event) => {
                            let key = match &*event {
                                DownloadEvent::StatusChanged { id, .. } => Some(id.clone()),
                                _ => None,
                            };
                            match key {
                                // Collapse rapid-fire per-row updates into
                                // the latest one, flushed on the next tick.
                                Some(id) => {
                                    pending_status.insert(id, *event);
                                }
                                None => (self.sink)(ServiceEvent::Download(*event)),
                            }
                        }
                        DownloadRecvOutcome::Closed => {
                            downloads = None;
                        }
                        DownloadRecvOutcome::Lagged => {
                            // Events were dropped: the UI re-reads every row
                            // so no ring stays stuck on a stale state.
                            tracing::debug!("download events lagged; resyncing the UI");
                            (self.sink)(ServiceEvent::DownloadsResync);
                        }
                    }
                }
                _ = download_throttle.tick(), if !pending_status.is_empty() => {
                    for (_, event) in pending_status.drain() {
                        (self.sink)(ServiceEvent::Download(event));
                    }
                }
            }
        }
        self.shut_down().await;
    }

    /// Stops the proxy for good and withdraws the published [`Backend`]: the
    /// `watch` channel keeps its last value alive while any [`ProxyService`]
    /// clone exists, and that value holds the store's database connection,
    /// which "Remove all data" must see closed before deleting the file.
    async fn shut_down(&mut self) {
        self.stop().await;
        self.backend.send_replace(None);
    }

    async fn start(&mut self, restarted: bool) -> bool {
        (self.sink)(ServiceEvent::Starting);

        // The app opens storage here (before the proxy builder runs) so the
        // handle can be shared with the downloader. The builder normally
        // creates the data directories first, so ensure them here too;
        // otherwise storage would be opened before the data dir exists.
        if let Err(error) = self.paths.ensure_dirs() {
            tracing::warn!(%error, "could not create data directories before opening storage");
        }

        let storage = match Storage::open_async(self.paths.database_file()).await {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(%error, "could not open storage");
                (self.sink)(ServiceEvent::Failed(error.to_string()));
                return false;
            }
        };

        let mut builder = ProxyHandle::builder(self.config.clone())
            .paths(self.paths.clone())
            .storage(storage.clone());

        // Register the DRM downloader. Prefer a user-provided `device.wvd`
        // in the data directory; fall back to the bundled default device
        // so DRM downloads work out of the box.
        let wvd_path = self.paths.device_override_file();
        let drm_result = if wvd_path.exists() {
            match tokio::fs::read(&wvd_path).await {
                Ok(wvd_bytes) => {
                    tracing::info!("using device.wvd from data dir");
                    DrmDownloader::from_wvd_bytes(storage, &wvd_bytes)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read device.wvd; falling back to bundled device");
                    DrmDownloader::with_default_device(storage)
                }
            }
        } else {
            tracing::info!("using bundled default WVD device");
            DrmDownloader::with_default_device(storage)
        };

        match drm_result {
            Ok(drm) => {
                tracing::info!("DRM downloader enabled");
                builder = builder.downloader(drm);
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to initialize DRM downloader; DRM downloads disabled");
            }
        }

        let result = builder.start().await;
        match result {
            Ok(proxy) => {
                self.backend.send_replace(Some(Backend {
                    storage: proxy.storage().clone(),
                    library: proxy.library(),
                    ca: Arc::clone(proxy.certificate_authority()),
                    downloads: proxy.downloads().clone(),
                    library_sync: proxy.library_sync().clone(),
                }));
                (self.sink)(ServiceEvent::Started { restarted });
                (self.sink)(ServiceEvent::Status(proxy.status()));
                self.proxy = Some(proxy);
                true
            }
            Err(error) => {
                tracing::error!(%error, "could not start the proxy");
                (self.sink)(ServiceEvent::Failed(error.to_string()));
                false
            }
        }
    }

    async fn restart(&mut self, config: Config) {
        let previous = std::mem::replace(&mut self.config, config);
        self.stop().await;
        if !self.start(true).await && previous.listen_addr() != self.config.listen_addr() {
            tracing::warn!("falling back to the previous proxy address");
            self.config = previous;
            self.start(true).await;
        }
    }

    async fn stop(&mut self) {
        if let Some(proxy) = self.proxy.take()
            && let Err(error) = proxy.shutdown().await
        {
            tracing::warn!(%error, "proxy did not shut down cleanly");
        }
    }
}

/// Resolves when the watched status changes (`true`) or its sender is gone
/// (`false`); never resolves without a receiver.
async fn changed(rx: Option<&mut watch::Receiver<ProxyStatus>>) -> bool {
    match rx {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Resolves with the new status when a library sync run finished (the only
/// change that leaves `running` false), or `None` when the sender is gone;
/// never resolves without a receiver. Cancel-safe: an unseen change stays
/// pending in the receiver until this future observes it.
async fn sync_finished(rx: Option<&mut watch::Receiver<SyncStatus>>) -> Option<SyncStatus> {
    let Some(rx) = rx else {
        return std::future::pending().await;
    };
    loop {
        rx.changed().await.ok()?;
        let status = rx.borrow_and_update().clone();
        if !status.running {
            return Some(status);
        }
    }
}

/// Outcome of polling the broadcast receiver for the download queue.
enum DownloadRecvOutcome {
    Event(Box<DownloadEvent>),
    Closed,
    Lagged,
}

/// Resolves on the next download event, closed or lag; never resolves without
/// a receiver so it can be `select!`ed alongside other sources.
async fn recv_download(rx: Option<&mut broadcast::Receiver<DownloadEvent>>) -> DownloadRecvOutcome {
    match rx {
        Some(rx) => match rx.recv().await {
            Ok(event) => DownloadRecvOutcome::Event(Box::new(event)),
            Err(broadcast::error::RecvError::Closed) => DownloadRecvOutcome::Closed,
            Err(broadcast::error::RecvError::Lagged(_)) => DownloadRecvOutcome::Lagged,
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cider_offline_core::{AppPaths, Config};

    use super::*;
    use crate::cleanup::{self, CaFolder, Report};

    /// The in-app "Remove all data" order from `main` on a portable root:
    /// start the proxy, shut the actor down while a UI clone of the service
    /// is still alive (as `AppContext` holds one), stop the runtime, then
    /// delete the files.
    fn shut_down_then_remove_files(root: &Path, mut config: Config) -> Report {
        config.proxy.port = 0;
        let paths = AppPaths::under(root);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let service = ProxyService::spawn(rt.handle(), config, paths.clone(), |_| {});
        let ui_clone = service.clone();
        let backend = rt.block_on(service.wait_backend()).unwrap();
        rt.block_on(backend.storage.list_tracks()).unwrap();
        drop(backend);

        rt.block_on(service.shutdown());
        drop(service);
        rt.shutdown_timeout(Duration::from_secs(2));
        let report = cleanup::remove_files(&paths, CaFolder::Remove, false);
        drop(ui_clone);
        report
    }

    /// On Windows an open database connection would make deleting
    /// `store.sqlite` fail.
    #[test]
    fn shutdown_should_close_the_store_so_remove_files_can_delete_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();

        let report = shut_down_then_remove_files(&root, Config::default());

        assert!(
            report.is_ok() && !root.join("store.sqlite").exists() && root.is_dir(),
            "{report}"
        );
    }

    /// The same against a real portable copy: set `CIDER_REMOVAL_ROOT` to
    /// its `data` folder (never the real app folders: this deletes them).
    /// Uses that copy's `config/config.toml`, on a free port.
    #[test]
    #[ignore = "deletes the app files of the portable root named by CIDER_REMOVAL_ROOT"]
    fn shutdown_then_remove_files_on_a_portable_copy() {
        let root = std::path::PathBuf::from(std::env::var_os("CIDER_REMOVAL_ROOT").unwrap());
        let config = Config::load_or_default(&AppPaths::under(&root).config_file()).unwrap();

        let report = shut_down_then_remove_files(&root, config);

        std::io::Write::write_all(&mut std::io::stdout(), report.to_string().as_bytes()).unwrap();
        assert!(
            report.is_ok() && !root.join("store.sqlite").exists() && root.is_dir(),
            "{report}"
        );
    }
}
