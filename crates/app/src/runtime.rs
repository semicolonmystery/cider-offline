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
    AppPaths, AutoCacheMode, CiderSpatial, Config, DownloadEvent, DownloadFormat, DownloadQueue,
    Library, LibrarySync, Mode, ProxyHandle, ProxyStatus, SongsPage, Storage, SyncStatus,
    UnlinkedCleanup, queue_kept_collections,
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
/// How often Cider's spatial audio setting is read again (it decides which
/// version of a song Cider asks for; see `cider_offline_core` routing).
const CIDER_SPATIAL_REFRESH: Duration = Duration::from_secs(30);
/// How often the actor checks whether an automatic clean-up of unlinked
/// downloads is due.
const UNLINKED_CHECK: Duration = Duration::from_mins(10);

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
    /// Cider's Apple Music tokens were just captured or changed.
    SessionChanged,
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
    SetDownloadFormat(DownloadFormat),
    SetPlayOtherMix(bool),
    SetUnlinkedCleanup(UnlinkedCleanup, u32),
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

    /// Switches which mixes downloads fetch (no restart).
    pub fn set_download_format(&self, format: DownloadFormat) {
        self.send(Command::SetDownloadFormat(format));
    }

    /// Turns "Play the other downloaded mix" on or off (no restart).
    pub fn set_play_other_downloaded_mix(&self, enabled: bool) {
        self.send(Command::SetPlayOtherMix(enabled));
    }

    /// Sets what happens to unlinked downloads and how many hours apart the
    /// automatic check runs (no restart).
    pub fn set_unlinked_cleanup(&self, mode: UnlinkedCleanup, hours: u32) {
        self.send(Command::SetUnlinkedCleanup(mode, hours));
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
            Command::SetDownloadFormat(format) => {
                self.config.download_format = format;
                if let Some(proxy) = &self.proxy {
                    proxy.set_download_format(format);
                }
            }
            Command::SetPlayOtherMix(enabled) => {
                self.config.play_other_downloaded_mix = enabled;
                if let Some(proxy) = &self.proxy {
                    proxy.set_play_other_downloaded_mix(enabled);
                }
            }
            Command::SetUnlinkedCleanup(mode, hours) => {
                self.config.unlinked_downloads = mode;
                self.config.unlinked_cleanup_hours = hours;
            }
            Command::Restart(_) | Command::Shutdown(_) => {}
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.start(false).await;
        let mut status = self.proxy.as_ref().map(ProxyHandle::subscribe);
        let mut downloads = self.proxy.as_ref().map(|p| p.downloads().events());
        let mut library_sync = self.proxy.as_ref().map(|p| p.library_sync().subscribe());
        let mut tokens = self.subscribe_tokens();
        let mut throttle = tokio::time::interval(STATUS_THROTTLE);
        throttle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut download_throttle = tokio::time::interval(DOWNLOAD_STATUS_THROTTLE);
        download_throttle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = false;
        let mut pending_status: HashMap<String, DownloadEvent> = HashMap::new();
        // The first tick fires at once: the setting is read right after start.
        let mut cider_spatial = tokio::time::interval(CIDER_SPATIAL_REFRESH);
        cider_spatial.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut unlinked_check = tokio::time::interval(UNLINKED_CHECK);
        unlinked_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first automatic clean-up runs one check interval after start,
        // then every configured number of hours.
        let mut last_cleanup = tokio::time::Instant::now();

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
                            tokens = self.subscribe_tokens();
                            dirty = true;
                            self.refresh_cider_spatial().await;
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
                changed = changed(tokens.as_mut()) => {
                    if changed {
                        (self.sink)(ServiceEvent::SessionChanged);
                    } else {
                        tokens = None;
                    }
                }
                finished = sync_finished(library_sync.as_mut()) => match finished {
                    Some(status) => {
                        if status.last_error.is_none() {
                            self.queue_kept_collections();
                        }
                        (self.sink)(ServiceEvent::LibrarySync(status));
                    }
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
                            if let Some(event) = hold_back(&mut pending_status, *event) {
                                (self.sink)(ServiceEvent::Download(event));
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
                _ = cider_spatial.tick() => self.refresh_cider_spatial().await,
                _ = unlinked_check.tick() => self.clean_up_unlinked_if_due(&mut last_cleanup),
            }
        }
        self.shut_down().await;
    }

    /// Watches the running proxy's captured tokens (see
    /// [`ServiceEvent::SessionChanged`]).
    fn subscribe_tokens(&self) -> Option<watch::Receiver<u64>> {
        self.proxy
            .as_ref()
            .map(|p| p.library_sync().subscribe_tokens())
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

    /// "Keep downloaded": queues the songs added to kept collections by the
    /// sync that just finished. Runs on its own task.
    fn queue_kept_collections(&self) {
        let Some(proxy) = &self.proxy else { return };
        let storage = proxy.storage().clone();
        let queue = proxy.downloads().clone();
        tokio::spawn(async move {
            match queue_kept_collections(&storage, &queue).await {
                Ok(0) => {}
                Ok(count) => tracing::info!(count, "queued songs added to kept collections"),
                Err(error) => tracing::warn!(%error, "could not queue songs of kept collections"),
            }
        });
    }

    /// The automatic clean-up of unlinked downloads, when it is on and due
    /// (`last` is when it last ran). Runs on its own task.
    fn clean_up_unlinked_if_due(&self, last: &mut tokio::time::Instant) {
        let every = Duration::from_hours(u64::from(self.config.unlinked_cleanup_hours()));
        if self.config.unlinked_downloads != UnlinkedCleanup::Automatic
            || last.elapsed() + UNLINKED_CHECK < every
        {
            return;
        }
        *last = tokio::time::Instant::now();
        let Some(proxy) = &self.proxy else { return };
        let queue = proxy.downloads().clone();
        tokio::spawn(async move {
            match queue.clean_up_unlinked().await {
                Ok(report) if report.files > 0 => {
                    tracing::info!(
                        files = report.files,
                        bytes = report.bytes,
                        "automatic clean-up of unlinked downloads"
                    );
                }
                Ok(_) => tracing::debug!("no unlinked downloads"),
                Err(error) => tracing::warn!(%error, "automatic clean-up failed"),
            }
        });
    }

    /// Reads Cider's spatial audio setting (`audio.atmos.enabled` in its
    /// `spa-config.yml`) and hands it to the proxy; unknown when no Cider
    /// config is found.
    async fn refresh_cider_spatial(&mut self) {
        if self.proxy.is_none() {
            return;
        }
        let chosen = self.config.cider_config_path.clone();
        let setting = tokio::task::spawn_blocking(move || {
            crate::cider::locate(chosen.as_deref())
                .ok()
                .and_then(|path| crate::cider::spatial_audio_enabled(&path))
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "reading Cider's spatial audio setting failed");
            None
        });
        if let Some(proxy) = &self.proxy {
            proxy.set_cider_spatial(CiderSpatial::from_setting(setting));
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

/// Collapses rapid-fire per-row status updates: a `StatusChanged` is held in
/// `pending` (the latest per row, flushed on the next tick) and `None` is
/// returned; any other event is returned to forward now. A row re-added or
/// removed drops its held-back status, which belongs to its previous life
/// and must not be flushed after it.
fn hold_back(
    pending: &mut HashMap<String, DownloadEvent>,
    event: DownloadEvent,
) -> Option<DownloadEvent> {
    match &event {
        DownloadEvent::StatusChanged { id, .. } => {
            pending.insert(id.clone(), event);
            return None;
        }
        DownloadEvent::Added(record) => {
            pending.remove(&record.id);
        }
        DownloadEvent::Removed(id) => {
            pending.remove(id);
        }
        _ => {}
    }
    Some(event)
}

/// Resolves when the watched value changes (`true`) or its sender is gone
/// (`false`); never resolves without a receiver.
async fn changed<T>(rx: Option<&mut watch::Receiver<T>>) -> bool {
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
    use cider_offline_core::{DownloadEvent, DownloadStatus};

    fn status(id: &str) -> DownloadEvent {
        DownloadEvent::StatusChanged {
            id: id.to_owned(),
            status: DownloadStatus::Downloading,
            progress: 0.5,
            message: None,
            stage: None,
            attempt: 1,
        }
    }

    #[test]
    fn a_removed_row_should_drop_its_held_back_status() {
        let mut pending = std::collections::HashMap::new();
        assert!(super::hold_back(&mut pending, status("7")).is_none());
        assert!(super::hold_back(&mut pending, DownloadEvent::Removed("7".to_owned())).is_some());
        assert_eq!(pending.len(), 0);
    }

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
