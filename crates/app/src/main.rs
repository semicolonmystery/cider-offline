//! Cider Offline desktop app: Slint window + system tray around the
//! proxy engine in `cider-offline-core`.
//!
//! Threading: the main thread runs the Slint event loop (required on macOS)
//! and owns all UI state. A multi-threaded tokio runtime on background
//! threads runs the proxy actor ([`runtime`]) and every storage / network /
//! file operation. Background threads talk to the UI through a channel of
//! [`ui::UiEvent`]s drained by a `slint::spawn_local` task.

// Release builds are GUI apps on Windows (no console window).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cider;
mod cleanup;
mod headless_check;
mod instance;
mod library_check;
mod logging;
mod offline_check;
mod perf_check;
mod platform;
mod runtime;
mod settings;
mod startup;
mod tray;
mod tray_host;
mod trust;
mod ui;

use std::rc::Rc;
use std::time::Duration;

use anyhow::Context as _;
use cider_offline_core::{AppPaths, Config};
use slint::ComponentHandle as _;
use tokio::sync::mpsc;

use crate::instance::Instance;
use crate::runtime::{ProxyService, ServiceEvent};
use crate::tray_host::StartMode;
use crate::ui::{AppContext, AppWindow, PendingActivation, UiEvent};

/// Upper bound for the graceful proxy shutdown on quit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(8);

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Before any logging: `--version` must not create log folders (in a
    // portable `data/` either).
    if startup::version_requested(&args) {
        return startup::print_version();
    }
    let discovered = startup::discover_paths();
    // Cleanup runs log to stdout only: the log file would sit in the folder
    // they delete (and a dry run must not create it).
    let file_logging = !cleanup::requested(&args);
    let log_guard = logging::init(discovered.as_ref().ok().filter(|_| file_logging));
    let paths = discovered?;
    startup::warn_if_translocated();

    if let Some(result) = run_command(&args) {
        return result;
    }

    let config = load_config(&paths);
    if let Some(result) = cleanup::run_cli(&args, &paths, &config) {
        return result;
    }

    // Held until the end of `main`: dropping it releases the instance lock.
    let mut primary = match instance::acquire(&paths.instance_lock_file()) {
        Ok(Instance::Primary(primary)) => Some(primary),
        Ok(Instance::Secondary) => {
            tracing::info!("already running; exiting");
            return Ok(());
        }
        Err(error) => {
            tracing::warn!(%error, "single-instance check failed; continuing");
            None
        }
    };

    // Created before anything slow so a second launch during startup is not
    // lost: the listener exists from `acquire`, and `serve` starts here,
    // buffering `show` requests in this channel until the UI drains it.
    let (events, mut event_rx) = mpsc::unbounded_channel::<UiEvent>();
    serve_activation(primary.as_mut(), &events);

    let start_hidden =
        config.start_minimized || std::env::args().any(|a| a == settings::MINIMIZED_FLAG);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("cider-offline-rt")
        .build()
        .context("creating the tokio runtime")?;

    let activation = select_backend()?;
    let app = AppWindow::new().context("creating the main window")?;

    let service = {
        let events = events.clone();
        // Always start in Passthrough: the first certificate check switches
        // to Active when nothing blocks it (see `ui::overview::ModePolicy`),
        // so Cider never meets an untrusted certificate at startup.
        let mut start = config.clone();
        start.mode = cider_offline_core::Mode::Passthrough;
        ProxyService::spawn(rt.handle(), start, paths.clone(), move |event| {
            // Fails only after the UI is gone, i.e. during shutdown.
            let ui_event = match event {
                ServiceEvent::Download(e) => UiEvent::Download(e),
                other => UiEvent::Service(other),
            };
            let _ = events.send(ui_event);
        })
    };
    let ctx = AppContext::new(
        &app,
        rt.handle().clone(),
        service.clone(),
        paths.clone(),
        config,
        activation,
    );
    ui::bind(&ctx);

    let tray_available = {
        let events = events.clone();
        tray_host::watch(move |available| {
            let _ = events.send(UiEvent::TrayHost(available));
        })
    };
    ctx.set_tray_available(tray_available);
    {
        let events = events.clone();
        tray::set_event_handler(move |action| {
            let _ = events.send(UiEvent::Tray(action));
        });
    }
    drop(events);

    {
        let ctx = Rc::clone(&ctx);
        slint::spawn_local(async move {
            while let Some(event) = event_rx.recv().await {
                ctx.handle(event);
            }
        })
        .context("starting the UI event task")?;
    }

    install_window_handlers(&app, &ctx);

    show_initial(&app, tray_host::start_mode(start_hidden, tray_available))?;
    slint::run_event_loop_until_quit().context("running the UI event loop")?;

    tracing::info!("quitting; stopping the proxy");
    drop(app);
    stop_proxy(&rt, &service);
    let removal = ctx.data_removal_requested();
    // Let go of every handle that could still reach the store, so its
    // database connection is closed before "Remove all data" deletes the
    // file (Windows refuses to delete an open file). The actor already
    // withdrew its `Backend`; the runtime shutdown drops any task still
    // holding a clone.
    drop(ctx);
    drop(service);
    rt.shutdown_timeout(Duration::from_secs(2));
    // Release the instance lock only once the proxy is fully stopped.
    drop(primary);
    if let Some(ca) = removal {
        // Stop the log file writer first: its open file sits in the logs
        // folder about to be deleted. Later events go to stdout only.
        drop(log_guard);
        cleanup::remove_files_after_quit(&paths, ca);
    }
    Ok(())
}

/// Stops the proxy gracefully, waiting at most [`SHUTDOWN_TIMEOUT`].
fn stop_proxy(rt: &tokio::runtime::Runtime, service: &ProxyService) {
    // `timeout` creates its timer when called, which needs the runtime's
    // context: build it inside the future `block_on` drives, not outside.
    if rt
        .block_on(async { tokio::time::timeout(SHUTDOWN_TIMEOUT, service.shutdown()).await })
        .is_err()
    {
        tracing::warn!("proxy shutdown timed out");
    }
}

/// Answers second launches from now on, queueing their `show` requests as
/// [`UiEvent::ShowWindow`] until the UI drains `events`.
fn serve_activation(
    primary: Option<&mut instance::Primary>,
    events: &mpsc::UnboundedSender<UiEvent>,
) {
    if let Some(primary) = primary {
        let events = events.clone();
        primary.serve(move |activation_token| {
            let _ = events.send(UiEvent::ShowWindow { activation_token });
        });
    }
}

/// Selects the UI backend with a window-attributes hook that hands a second
/// launch's activation token (Linux) to the winit window when it is created,
/// so the compositor lets it take focus. Returns the token slot.
fn select_backend() -> anyhow::Result<PendingActivation> {
    let activation = PendingActivation::default();
    let hook_activation = activation.clone();
    slint::BackendSelector::new()
        .with_winit_window_attributes_hook(move |attributes| hook_activation.apply(attributes))
        .select()
        .context("selecting the UI backend")?;
    Ok(activation)
}

/// Installs the close handler and the first-iteration tray setup.
fn install_window_handlers(app: &AppWindow, ctx: &Rc<AppContext>) {
    {
        let ctx = Rc::clone(ctx);
        app.window().on_close_requested(move || {
            // Hide to tray; without a tray, closing the window quits
            // (the minimize button just minimizes). Both already hide the
            // window; `HideWindow` repeats that harmlessly.
            if ctx.has_tray() {
                ctx.hide_to_tray();
            } else {
                ctx.quit();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // tray-icon requires a running event loop (macOS: on the main thread),
    // so create it from the first event-loop iteration.
    // A hidden start also drops the Dock icon (macOS); winit only applies
    // its activation policy once the app has launched, so do it here too.
    {
        let ctx = Rc::clone(ctx);
        slint::Timer::single_shot(Duration::ZERO, move || {
            ctx.create_tray();
            if ctx.ui.upgrade().is_some_and(|ui| !ui.window().is_visible()) {
                platform::set_dock_visible(false);
            }
        });
    }
}

/// Shows the window as the start mode asks.
fn show_initial(app: &AppWindow, mode: StartMode) -> anyhow::Result<()> {
    match mode {
        StartMode::Hidden => {}
        StartMode::Visible => app.show().context("showing the main window")?,
        StartMode::Minimized => {
            app.show().context("showing the main window")?;
            app.window().set_minimized(true);
        }
    }
    Ok(())
}

/// Runs a command-line check, if one is given, instead of the app.
/// (`--version` is handled earlier, before logging starts.)
fn run_command(args: &[String]) -> Option<anyhow::Result<()>> {
    if let Some(options) = library_check::parse(args) {
        return Some(library_check::exit_with(library_check::run(&options)));
    }
    if let Some(options) = offline_check::parse(args) {
        return Some(offline_check::exit_with(offline_check::run(&options)));
    }
    if let Some(check) = headless_check::parse(args) {
        return Some(headless_check::run(check));
    }
    if let Some(options) = perf_check::parse(args) {
        return Some(perf_check::run(&options));
    }
    if let Some(options) = ui::shot::parse(args) {
        return Some(ui::shot::run(&options));
    }
    None
}

/// Loads `config.toml`, falling back to defaults (without overwriting the
/// file) when it is unreadable so a typo never prevents the app from starting.
fn load_config(paths: &AppPaths) -> Config {
    let path = paths.config_file();
    match Config::load_or_default(&path) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "invalid config; using defaults");
            Config::default()
        }
    }
}
