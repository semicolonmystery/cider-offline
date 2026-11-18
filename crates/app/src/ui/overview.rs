//! Overview page: mode, capture, status, CA trust and the activity feed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use cider_offline_core::{Mode, ProxyStatus};
use slint::{ComponentHandle as _, Model as _, ModelRc, VecModel};

use super::{ActivityRow, AppContext, ProxyState, format};
use crate::runtime::ServiceEvent;
use crate::trust::{self, TrustState};

/// Maximum number of activity lines kept.
const ACTIVITY_LIMIT: usize = 100;
/// How often accumulated traffic counts are summarised in the feed.
const TRAFFIC_SUMMARY_EVERY: Duration = Duration::from_secs(5);

/// Overview state owned by the UI thread.
#[derive(Default)]
pub struct OverviewData {
    activity: Rc<VecModel<ActivityRow>>,
    previous: RefCell<Option<ProxyStatus>>,
    /// Requests / locally served since the last summary line.
    pending: Cell<(u64, u64)>,
    summary_timer: slint::Timer,
}

/// Wires the Overview callbacks.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<ProxyState>();
    state.set_activity(ModelRc::from(Rc::clone(&ctx.overview.activity)));
    {
        let config = ctx.config();
        state.set_active(config.mode == Mode::Active);
        state.set_capture(config.capture);
        state.set_address(config.listen_addr().to_string().into());
    }
    state.set_translocation_hint(crate::startup::running_translocated());

    let c = Rc::clone(ctx);
    state.on_set_active(move |active| set_active(&c, active));
    let c = Rc::clone(ctx);
    state.on_set_capture(move |capture| set_capture(&c, capture));
    let c = Rc::clone(ctx);
    state.on_install_ca(move || install_ca(&c));
    let c = Rc::clone(ctx);
    state.on_recheck_ca(move || check_ca(&c));
    let c = Rc::clone(ctx);
    state.on_dismiss_tray_hint(move || {
        c.update_config(|config| config.tray_hint_dismissed = true);
        c.with_ui(|ui| ui.global::<ProxyState>().set_tray_hint(false));
    });

    let weak = Rc::downgrade(ctx);
    ctx.overview.summary_timer.start(
        slint::TimerMode::Repeated,
        TRAFFIC_SUMMARY_EVERY,
        move || {
            if let Some(ctx) = weak.upgrade() {
                flush_traffic_summary(&ctx);
            }
        },
    );
}

/// Switches between Active and Passthrough everywhere (UI, proxy, config, tray).
pub fn set_active(ctx: &Rc<AppContext>, active: bool) {
    let mode = if active {
        Mode::Active
    } else {
        Mode::Passthrough
    };
    ctx.with_ui(|ui| ui.global::<ProxyState>().set_active(active));
    ctx.service.set_mode(mode);
    ctx.update_config(|config| config.mode = mode);
    ctx.sync_tray();
}

/// Enables or disables capture everywhere (UI, proxy, config, tray).
pub fn set_capture(ctx: &Rc<AppContext>, capture: bool) {
    ctx.with_ui(|ui| ui.global::<ProxyState>().set_capture(capture));
    ctx.service.set_capture(capture);
    ctx.update_config(|config| config.capture = capture);
    ctx.sync_tray();
}

/// Applies an event from the proxy actor.
pub fn on_service_event(ctx: &Rc<AppContext>, event: ServiceEvent) {
    match event {
        ServiceEvent::Starting => ctx.with_ui(|ui| {
            let state = ui.global::<ProxyState>();
            state.set_starting(true);
            state.set_running(false);
        }),
        ServiceEvent::Started { restarted } => {
            ctx.with_ui(|ui| {
                let state = ui.global::<ProxyState>();
                state.set_starting(false);
                state.set_last_error("".into());
            });
            push_activity(
                ctx,
                "info",
                if restarted {
                    "Proxy restarted"
                } else {
                    "Proxy started"
                },
            );
            if !restarted {
                check_ca(ctx);
                super::library::reload(ctx);
                super::cider_page::refresh(ctx);
            }
        }
        ServiceEvent::Failed(error) => {
            ctx.with_ui(|ui| {
                let state = ui.global::<ProxyState>();
                state.set_starting(false);
                state.set_running(false);
                state.set_last_error(format!("Could not start the proxy: {error}").into());
            });
            push_activity(ctx, "error", &format!("Could not start the proxy: {error}"));
            super::settings_page::on_start_failed(ctx, &error);
            ctx.sync_tray();
        }
        ServiceEvent::Status(status) => apply_status(ctx, status),
        // Download events are split off in `main` into `UiEvent::Download`
        // and routed to the Downloads page; this arm is unreachable in
        // practice but keeps the match exhaustive.
        // Library sync results are handled by the Library page
        // (`AppContext::handle`).
        ServiceEvent::Download(_)
        | ServiceEvent::LibrarySync(_)
        | ServiceEvent::DownloadsResync => {}
    }
}

fn apply_status(ctx: &Rc<AppContext>, status: ProxyStatus) {
    let previous = ctx.overview.previous.take();
    describe_changes(ctx, previous.as_ref(), &status);

    ctx.with_ui(|ui| {
        let state = ui.global::<ProxyState>();
        state.set_active(status.mode == Mode::Active);
        state.set_capture(status.capture);
        state.set_online(status.online);
        state.set_running(status.running);
        state.set_address(status.listening.to_string().into());
        state.set_requests_seen(format::count(status.counters.requests_seen).into());
        state.set_served_locally(format::count(status.counters.served_locally).into());
        if let Some(error) = &status.last_error {
            state.set_last_error(error.as_str().into());
        }
    });
    super::settings_page::on_listening(ctx, status.listening.port());
    ctx.overview.previous.replace(Some(status));
    ctx.sync_tray();
}

/// Adds feed lines for notable changes between two status snapshots.
fn describe_changes(ctx: &Rc<AppContext>, previous: Option<&ProxyStatus>, now: &ProxyStatus) {
    let Some(previous) = previous else { return };
    if previous.online != now.online {
        if now.online {
            push_activity(ctx, "info", "Apple Music is reachable again");
        } else {
            push_activity(ctx, "warn", "Offline: serving from the local cache");
        }
    }
    if previous.mode != now.mode {
        let text = match now.mode {
            Mode::Active => "Switched to Active",
            Mode::Passthrough => "Switched to Passthrough",
        };
        push_activity(ctx, "info", text);
    }
    if previous.capture != now.capture {
        push_activity(
            ctx,
            "info",
            if now.capture {
                "Capture enabled"
            } else {
                "Capture disabled"
            },
        );
    }
    if previous.running && !now.running {
        push_activity(ctx, "error", "Proxy stopped");
    }
    if now.last_error.is_some()
        && previous.last_error != now.last_error
        && let Some(error) = &now.last_error
    {
        push_activity(ctx, "error", error);
    }

    let requests = now
        .counters
        .requests_seen
        .saturating_sub(previous.counters.requests_seen);
    let local = now
        .counters
        .served_locally
        .saturating_sub(previous.counters.served_locally);
    let (pending_requests, pending_local) = ctx.overview.pending.get();
    ctx.overview
        .pending
        .set((pending_requests + requests, pending_local + local));
}

fn flush_traffic_summary(ctx: &Rc<AppContext>) {
    let (requests, local) = ctx.overview.pending.replace((0, 0));
    if requests == 0 && local == 0 {
        return;
    }
    let noun = if requests == 1 { "request" } else { "requests" };
    let text = if local > 0 {
        format!("{requests} {noun}, {local} served locally")
    } else {
        format!("{requests} {noun}")
    };
    push_activity(ctx, "info", &text);
}

/// Prepends a line to the activity feed.
pub fn push_activity(ctx: &AppContext, level: &str, text: &str) {
    let model = &ctx.overview.activity;
    model.insert(
        0,
        ActivityRow {
            time: chrono::Local::now().format("%H:%M:%S").to_string().into(),
            text: text.into(),
            level: level.into(),
        },
    );
    while model.row_count() > ACTIVITY_LIMIT {
        model.remove(model.row_count() - 1);
    }
}

/// Re-checks whether the CA is trusted.
pub fn check_ca(ctx: &Rc<AppContext>) {
    ctx.with_ui(|ui| ui.global::<ProxyState>().set_trust_state("checking".into()));
    ctx.with_backend(
        |backend| trust::check(backend.ca),
        |ctx, state| {
            let state =
                state.unwrap_or_else(|| TrustState::Unknown("the proxy is not running".to_owned()));
            show_trust(ctx, &state, None);
        },
    );
}

fn install_ca(ctx: &Rc<AppContext>) {
    set_trust_busy(ctx, true);
    ctx.with_backend(
        |backend| async move {
            trust::install(Arc::clone(&backend.ca)).await?;
            Ok::<_, String>(trust::check(backend.ca).await)
        },
        |ctx, result| {
            set_trust_busy(ctx, false);
            match result {
                Some(Ok(state)) => {
                    push_activity(ctx, "info", "Certificate installed");
                    show_trust(ctx, &state, None);
                }
                Some(Err(error)) => {
                    push_activity(
                        ctx,
                        "error",
                        &format!("Certificate install failed: {error}"),
                    );
                    show_trust(ctx, &TrustState::Unknown(error.clone()), Some(&error));
                }
                None => show_trust(
                    ctx,
                    &TrustState::Unknown(String::new()),
                    Some("The proxy is not running."),
                ),
            }
        },
    );
}

/// Removes the CA from the trust store (Settings page).
pub fn uninstall_ca(ctx: &Rc<AppContext>) {
    set_trust_busy(ctx, true);
    ctx.with_backend(
        |backend| async move {
            trust::uninstall(Arc::clone(&backend.ca)).await?;
            Ok::<_, String>(trust::check(backend.ca).await)
        },
        |ctx, result| {
            set_trust_busy(ctx, false);
            let message = match result {
                Some(Ok(state)) => {
                    show_trust(ctx, &state, None);
                    "Certificate removed from the trust store.".to_owned()
                }
                Some(Err(error)) => format!("Could not remove the certificate: {error}"),
                None => "The proxy is not running.".to_owned(),
            };
            push_activity(ctx, "info", &message);
            super::settings_page::show_message(ctx, &message);
        },
    );
}

fn set_trust_busy(ctx: &AppContext, busy: bool) {
    ctx.with_ui(|ui| ui.global::<ProxyState>().set_trust_busy(busy));
}

fn show_trust(ctx: &AppContext, state: &TrustState, error: Option<&str>) {
    let message = match (state, error) {
        (_, Some(error)) => error.to_owned(),
        (
            TrustState::Trusted {
                system: Some(false),
            },
            None,
        ) => "Trusted for Cider. System-wide trust was not installed (optional).".to_owned(),
        (TrustState::Trusted { .. }, None) => String::new(),
        (TrustState::Untrusted, None) => {
            "Cider will reject the proxy's HTTPS traffic until the certificate is trusted."
                .to_owned()
        }
        (TrustState::Unknown(reason), None) => format!("Could not check the trust store: {reason}"),
    };
    ctx.with_ui(|ui| {
        let ps = ui.global::<ProxyState>();
        ps.set_trust_state(state.ui_key().into());
        ps.set_trust_message(message.into());
    });
}
