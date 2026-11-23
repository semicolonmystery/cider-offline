//! Main-thread application context shared by all UI bindings.

use std::cell::{Cell, Ref, RefCell};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use cider_offline_core::{AppPaths, Config, DownloadEvent, Mode};
use slint::ComponentHandle as _;
use slint::winit_030::{WinitWindowAccessor as _, winit};
use tokio::runtime::Handle;

use super::activation::{self, PendingActivation, Raise};
use super::{AppWindow, ProxyState};
use crate::platform;
use crate::runtime::{Backend, ProxyService, ServiceEvent};
use crate::settings::{self, Autostart};
use crate::tray::{Connection, Tray, TrayAction, TrayState};
use crate::tray_host::{self, HostChange};

/// Events delivered to the UI thread from background threads.
#[derive(Debug)]
pub enum UiEvent {
    /// From the proxy actor.
    Service(ServiceEvent),
    /// From the download queue.
    Download(DownloadEvent),
    /// From the tray icon / menu.
    Tray(TrayAction),
    /// A system tray host appeared (`true`) or vanished (`false`).
    TrayHost(bool),
    /// A second launch asked us to show the window.
    ShowWindow {
        /// The second launch's desktop activation token (Linux), if any.
        activation_token: Option<String>,
    },
}

/// State owned by the UI thread. Shared as `Rc<AppContext>` by callbacks.
pub struct AppContext {
    /// The main window.
    pub ui: slint::Weak<AppWindow>,
    /// Tokio runtime running the proxy and all I/O.
    pub rt: Handle,
    /// Control handle of the proxy actor.
    pub service: ProxyService,
    /// App directories.
    pub paths: AppPaths,
    /// Launch-at-login registration, if supported.
    pub autostart: Option<Arc<Autostart>>,
    /// Activation token for the next window creation (see `main`'s hook).
    activation: PendingActivation,
    pub(super) dialog: super::dialogs::DialogSlot,
    config: RefCell<Config>,
    tray: RefCell<Option<Tray>>,
    tray_available: Cell<bool>,
    /// "Remove all data" asked `main` to delete the files after shutdown.
    data_removal: Cell<Option<crate::cleanup::CaFolder>>,
    pub(super) library: super::library::LibraryData,
    pub(super) library_add: super::library_add::LibraryAddData,
    pub(super) overview: super::overview::OverviewData,
    pub(super) settings: super::settings_page::SettingsData,
    pub(super) downloads: super::downloads::DownloadsData,
}

impl AppContext {
    /// Creates the context. Call [`super::bind`] afterwards.
    pub fn new(
        ui: &AppWindow,
        rt: Handle,
        service: ProxyService,
        paths: AppPaths,
        config: Config,
        activation: PendingActivation,
    ) -> Rc<Self> {
        Rc::new(Self {
            ui: ui.as_weak(),
            rt,
            service,
            paths,
            autostart: Autostart::for_current_exe().map(Arc::new),
            activation,
            dialog: super::dialogs::DialogSlot::default(),
            config: RefCell::new(config),
            tray: RefCell::new(None),
            tray_available: Cell::new(true),
            data_removal: Cell::new(None),
            library: super::library::LibraryData::default(),
            library_add: super::library_add::LibraryAddData::default(),
            overview: super::overview::OverviewData::default(),
            settings: super::settings_page::SettingsData::default(),
            downloads: super::downloads::DownloadsData::default(),
        })
    }

    /// Current configuration.
    pub fn config(&self) -> Ref<'_, Config> {
        self.config.borrow()
    }

    /// Mutates the configuration and persists it in the background.
    pub fn update_config(self: &Rc<Self>, change: impl FnOnce(&mut Config)) {
        let snapshot = {
            let mut config = self.config.borrow_mut();
            change(&mut config);
            config.clone()
        };
        let path = self.paths.config_file();
        self.spawn(settings::save_config(snapshot, path), |ctx, result| {
            if let Err(error) = result {
                ctx.with_ui(|ui| {
                    ui.global::<super::SettingsState>()
                        .set_message(format!("Could not save settings: {error}").into());
                });
            }
        });
    }

    /// Runs `future` on the tokio runtime and `done` with its output on the
    /// UI thread.
    pub fn spawn<T, F>(self: &Rc<Self>, future: F, done: impl FnOnce(&Rc<Self>, T) + 'static)
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let task = self.rt.spawn(future);
        let ctx = Rc::clone(self);
        let spawned = slint::spawn_local(async move {
            match task.await {
                Ok(value) => done(&ctx, value),
                Err(error) => tracing::error!(%error, "background task failed"),
            }
        });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not schedule UI continuation");
        }
    }

    /// Like [`Self::spawn`], but first waits for the proxy services. `done`
    /// receives `None` if the proxy never started.
    pub fn with_backend<T, F, Fut>(
        self: &Rc<Self>,
        work: F,
        done: impl FnOnce(&Rc<Self>, Option<T>) + 'static,
    ) where
        T: Send + 'static,
        F: FnOnce(Backend) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let service = self.service.clone();
        self.spawn(
            async move {
                match service.wait_backend().await {
                    Some(backend) => Some(work(backend).await),
                    None => None,
                }
            },
            done,
        );
    }

    /// Runs `f` with the window if it still exists.
    pub fn with_ui(&self, f: impl FnOnce(&AppWindow)) {
        if let Some(ui) = self.ui.upgrade() {
            f(&ui);
        }
    }

    /// Shows and raises the main window.
    ///
    /// `activation_token` is a second launch's desktop activation token
    /// (Linux); the window-attributes hook hands it to the winit window when
    /// this call creates one. It is dropped when the winit window already
    /// exists.
    pub fn show_window(&self, activation_token: Option<String>) {
        self.with_ui(|ui| {
            let window = ui.window();
            let is_wayland = window
                .with_winit_window(activation::is_wayland)
                .unwrap_or(false);
            match activation::raise_strategy(is_wayland, window.is_visible()) {
                Raise::AttentionOnly => {
                    window.with_winit_window(|window| {
                        window.request_user_attention(Some(
                            winit::window::UserAttentionType::Informational,
                        ));
                    });
                }
                Raise::ShowAndFocus => {
                    // The hook hands the token over only when `show()` creates
                    // the winit window; skip it when one already exists.
                    let winit_window_exists = window.with_winit_window(|_| ()).is_some();
                    self.activation.offer(activation_token, winit_window_exists);
                    platform::set_dock_visible(true);
                    window.set_minimized(false);
                    if let Err(error) = window.show() {
                        tracing::error!(%error, "could not show the window");
                    }
                    platform::activate_app();
                    // `show()` is a no-op for an already visible window, so a
                    // window buried behind others would stay there; raise and
                    // focus it.
                    window.with_winit_window(winit::window::Window::focus_window);
                }
            }
        });
    }

    /// Hides the window to the tray (and, on macOS, removes the Dock icon).
    pub fn hide_to_tray(&self) {
        self.with_ui(|ui| {
            if let Err(error) = ui.hide() {
                tracing::debug!(%error, "could not hide the window");
            }
        });
        super::overview::window_hidden(self);
        platform::set_dock_visible(false);
    }

    /// Creates the tray icon. Must run inside the event loop. Does nothing
    /// while no tray host exists.
    pub fn create_tray(&self) {
        if !self.tray_available.get() || self.tray.borrow().is_some() {
            return;
        }
        match Tray::create(&self.tray_state()) {
            Ok(tray) => *self.tray.borrow_mut() = Some(tray),
            Err(error) => {
                tracing::error!(error = format!("{error:#}"), "tray icon unavailable");
                // Without a tray the window is the only way back in.
                self.show_window(None);
            }
        }
    }

    /// Records whether a tray host exists and shows the "no tray" hint
    /// accordingly. Call before the event loop starts and on every
    /// [`UiEvent::TrayHost`].
    pub fn set_tray_available(&self, available: bool) {
        self.tray_available.set(available);
        let hint = !available && !self.config().tray_hint_dismissed;
        self.with_ui(|ui| ui.global::<ProxyState>().set_tray_hint(hint));
    }

    /// Whether a working tray exists (otherwise closing the window quits).
    pub fn has_tray(&self) -> bool {
        self.tray_available.get() && self.tray.borrow().is_some()
    }

    /// The tray host appeared or vanished at runtime.
    fn on_tray_host(&self, available: bool) {
        self.set_tray_available(available);
        if available {
            self.create_tray();
        }
        let visible = self.ui.upgrade().is_some_and(|ui| ui.window().is_visible());
        if tray_host::on_host_change(available, visible) == HostChange::ShowWindow {
            self.show_window(None);
        }
    }

    /// Pushes the current proxy state to the tray.
    pub fn sync_tray(&self) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.update(&self.tray_state());
        }
    }

    fn tray_state(&self) -> TrayState {
        let Some(ui) = self.ui.upgrade() else {
            return TrayState {
                mode: Mode::Active,
                capture: false,
                connection: Connection::Stopped,
                address: String::new(),
            };
        };
        let state = ui.global::<ProxyState>();
        let connection = match (state.get_running(), state.get_online()) {
            (false, _) => Connection::Stopped,
            (true, true) => Connection::Online,
            (true, false) => Connection::Offline,
        };
        TrayState {
            mode: if state.get_active() {
                Mode::Active
            } else {
                Mode::Passthrough
            },
            capture: state.get_capture(),
            connection,
            address: state.get_address().into(),
        }
    }

    /// Dispatches an event from a background thread.
    pub fn handle(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::Service(event) => {
                if let ServiceEvent::Started { .. } | ServiceEvent::DownloadsResync = &event {
                    super::downloads::refresh_from_backend(self);
                }
                if let ServiceEvent::LibrarySync(status) = &event {
                    super::library::on_library_synced(self, status);
                }
                super::overview::on_service_event(self, event);
            }
            UiEvent::Download(event) => {
                super::downloads::on_event(self, event);
                // Mirror the queue onto the Library page's inline indicators.
                super::library::sync_download_status(self);
            }
            UiEvent::TrayHost(available) => self.on_tray_host(available),
            UiEvent::ShowWindow { activation_token } => {
                tracing::debug!(
                    has_activation_token = activation_token.is_some(),
                    "a second launch asked to show the window"
                );
                self.show_window(activation_token);
            }
            UiEvent::Tray(action) => match action {
                TrayAction::OpenWindow => self.show_window(None),
                TrayAction::TogglePassthrough => {
                    let active = self
                        .ui
                        .upgrade()
                        .is_some_and(|ui| ui.global::<ProxyState>().get_active());
                    super::overview::set_active(self, !active);
                }
                TrayAction::ToggleCapture => {
                    let capture = self
                        .ui
                        .upgrade()
                        .is_some_and(|ui| ui.global::<ProxyState>().get_capture());
                    super::overview::set_capture(self, !capture);
                }
                TrayAction::Quit => self.quit(),
            },
        }
    }

    /// Hides everything and leaves the event loop; `main` then stops the
    /// proxy gracefully.
    pub fn quit(&self) {
        self.with_ui(|ui| {
            if let Err(error) = ui.hide() {
                tracing::debug!(%error, "could not hide the window");
            }
        });
        // Dropping the tray removes the icon (and stops the `ksni` service on Linux).
        self.tray.borrow_mut().take();
        if let Err(error) = slint::quit_event_loop() {
            tracing::error!(%error, "could not quit the event loop");
        }
    }

    /// Asks `main` to delete all app files once the proxy has stopped. Call
    /// before [`Self::quit`].
    pub fn request_data_removal(&self, ca: crate::cleanup::CaFolder) {
        self.data_removal.set(Some(ca));
    }

    /// The CA folder choice passed to [`Self::request_data_removal`], if it
    /// was called.
    pub fn data_removal_requested(&self) -> Option<crate::cleanup::CaFolder> {
        self.data_removal.get()
    }

    /// Opens a folder in the OS file manager.
    pub fn open_folder(self: &Rc<Self>, dir: PathBuf) {
        self.spawn(
            async move {
                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&dir).ok();
                    opener::open(&dir)
                })
                .await
            },
            |ctx, result| {
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => error.to_string(),
                };
                ctx.with_ui(|ui| {
                    ui.global::<super::SettingsState>()
                        .set_message(format!("Could not open the folder: {error}").into());
                });
            },
        );
    }
}
