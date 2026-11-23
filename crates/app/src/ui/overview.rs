//! Overview page: the mode switch, the proxy status shared with Settings,
//! CA trust, the Apple Music session check, and the state of the window
//! that the page's animated background follows.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cider_offline_core::storage::TokenKind;
use cider_offline_core::{Mode, ProxyStatus};
use slint::winit_030::{EventResult, WinitWindowAccessor as _, winit};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use super::{AppContext, NoteSeed, OverviewAnim, ProxyState, format};
use crate::platform;
use crate::runtime::ServiceEvent;
use crate::trust::{self, TrustState};

/// How often a missing Apple Music session is looked for again while
/// traffic flows (a fallback: a captured token is reported at once, see
/// [`ServiceEvent::SessionChanged`]).
const SESSION_RECHECK: Duration = Duration::from_secs(5);
/// The window regaining focus re-reads the certificate, Cider's settings and
/// the session (they may have changed outside the app) at most this often.
const FOCUS_RECHECK: Duration = Duration::from_secs(2);
/// Notes on the magenta layer: a jittered grid of this many columns and rows.
const NOTE_COLUMNS: u8 = 11;
const NOTE_ROWS: u8 = 8;

/// Overview state owned by the UI thread.
#[derive(Default)]
pub struct OverviewData {
    /// The window has keyboard focus (from winit's `Focused` events).
    focused: Cell<bool>,
    /// When the session was last looked up; `None` while a lookup runs or
    /// once it was found.
    session_checked: Cell<Option<Instant>>,
    session_checking: Cell<bool>,
    /// A session check was asked for while one was running: run it again
    /// when that one is done, so a token stored meanwhile is not missed.
    session_again: Cell<bool>,
    /// When focus last re-read the outside state ([`FOCUS_RECHECK`]).
    focus_checked: Cell<Option<Instant>>,
    /// Whether the mode follows what blocks Active (see [`ModePolicy`]).
    policy: Cell<ModePolicy>,
    /// The proxy has started once and the startup mode was decided.
    first_start_done: Cell<bool>,
}

/// Who decides between Active and Passthrough.
///
/// The app starts in Passthrough and switches to Active as soon as nothing
/// blocks it (the certificate is trusted); while something blocks it, it
/// stays in Passthrough and the issue line says why. That automatic choice
/// keeps following the certificate (trusted again: Active; removed:
/// Passthrough) until the user flips the switch, which hands the mode over
/// to them for the rest of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModePolicy {
    /// The startup trust check has not answered yet.
    #[default]
    Pending,
    /// The mode follows the certificate.
    Automatic,
    /// The user chose the mode.
    User,
}

/// What the mode should become after a trust check, or `None` to leave it.
/// `trusted` is `Some(true)` for a trusted certificate, `Some(false)` for
/// one that is not installed, and `None` when the trust store could not be
/// read.
pub fn mode_after_trust_check(policy: ModePolicy, trusted: Option<bool>) -> Option<Mode> {
    match (policy, trusted) {
        // The user's choice stands; an unreadable store later changes nothing.
        (ModePolicy::User, _) | (ModePolicy::Automatic, None) => None,
        (_, Some(true)) => Some(Mode::Active),
        // Not installed, or unknown at startup: Active could break Cider, so
        // stay out of the way until a check says the certificate is trusted.
        (_, Some(false)) | (ModePolicy::Pending, None) => Some(Mode::Passthrough),
    }
}

/// The mode a (re)start of the proxy begins in: Passthrough until the first
/// trust check of this session answered (so Cider never meets an untrusted
/// certificate, also after a start that failed), else the mode the policy
/// set (`current`), which follows the certificate or is the user's choice.
pub fn start_mode(policy: ModePolicy, current: Mode) -> Mode {
    match policy {
        ModePolicy::Pending => Mode::Passthrough,
        ModePolicy::Automatic | ModePolicy::User => current,
    }
}

/// [`start_mode`] for a restart now.
pub fn restart_mode(ctx: &AppContext) -> Mode {
    let active = ctx
        .ui
        .upgrade()
        .is_some_and(|ui| ui.global::<ProxyState>().get_active());
    let current = if active {
        Mode::Active
    } else {
        Mode::Passthrough
    };
    start_mode(ctx.overview.policy.get(), current)
}

/// Wires the Overview callbacks.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<ProxyState>();
    {
        let config = ctx.config();
        // The proxy starts in Passthrough; the first trust check decides
        // (see `ModePolicy`).
        state.set_active(false);
        state.set_capture(config.capture);
        state.set_address(config.listen_addr().to_string().into());
    }
    state.set_translocation_hint(crate::startup::running_translocated());

    let anim = ui.global::<OverviewAnim>();
    anim.set_notes(ModelRc::new(VecModel::from(note_seeds())));
    anim.set_reduced_motion(platform::prefers_reduced_motion());
    watch_window(ctx, &ui);

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
}

/// Follows focus, minimizing and resizing of the window, so the Overview's
/// note layer exists only while the window is live (see [`update_live`]).
fn watch_window(ctx: &Rc<AppContext>, ui: &super::AppWindow) {
    let weak: Weak<AppContext> = Rc::downgrade(ctx);
    ui.window().on_winit_window_event(move |_, event| {
        let Some(ctx) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        match event {
            winit::event::WindowEvent::Focused(focused) => {
                ctx.overview.focused.set(*focused);
                defer_update_live(&ctx);
                if *focused {
                    recheck_outside_state(&ctx);
                }
            }
            winit::event::WindowEvent::Resized(_) | winit::event::WindowEvent::Occluded(_) => {
                defer_update_live(&ctx);
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

/// Re-evaluates [`update_live`] once Slint has handled the current event
/// (its minimized state is synced from winit while handling it).
fn defer_update_live(ctx: &Rc<AppContext>) {
    let weak = Rc::downgrade(ctx);
    slint::Timer::single_shot(Duration::ZERO, move || {
        if let Some(ctx) = weak.upgrade() {
            update_live(&ctx);
        }
    });
}

/// Sets `OverviewAnim.live`: the window is shown, focused and not minimized.
/// Turning live re-reads the OS reduced-motion setting, which can change
/// while the app runs.
pub fn update_live(ctx: &AppContext) {
    ctx.with_ui(|ui| {
        let window = ui.window();
        let live = window.is_visible() && !window.is_minimized() && ctx.overview.focused.get();
        let anim = ui.global::<OverviewAnim>();
        if anim.get_live() == live {
            return;
        }
        if live {
            anim.set_reduced_motion(platform::prefers_reduced_motion());
        }
        tracing::debug!(live, "overview animation");
        anim.set_live(live);
    });
}

/// The hidden window has no focus; called by `hide_to_tray`, before winit
/// reports it.
pub fn window_hidden(ctx: &AppContext) {
    ctx.overview.focused.set(false);
    update_live(ctx);
}

/// The notes of the magenta layer: a jittered grid, so they spread evenly
/// without looking placed, from a fixed seed (the same every start).
pub fn note_seeds() -> Vec<NoteSeed> {
    let mut random = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut notes = Vec::with_capacity(usize::from(NOTE_COLUMNS * NOTE_ROWS));
    for row in 0..NOTE_ROWS {
        for column in 0..NOTE_COLUMNS {
            let cell_x = 1.0 / f32::from(NOTE_COLUMNS);
            let cell_y = 1.0 / f32::from(NOTE_ROWS);
            notes.push(NoteSeed {
                x: (f32::from(column) + 0.15 + 0.7 * random.unit()) * cell_x,
                y: (f32::from(row) + 0.15 + 0.7 * random.unit()) * cell_y,
                size: 14.0 + 14.0 * random.unit(),
                glyph: random.below(4),
                vx: random.signed() * 3.0,
                vy: random.signed() * 3.0,
                amp: 6.0 + 10.0 * random.unit(),
                freq: 0.15 + 0.25 * random.unit(),
                phase: random.unit() * std::f32::consts::TAU,
                tilt: random.signed() * 16.0,
                light: random.unit() < 0.4,
            });
        }
    }
    notes
}

/// A tiny deterministic generator for the note layout (no dependency, no
/// need for quality).
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// 0..1.
    fn unit(&mut self) -> f32 {
        // The top 24 bits fit an f32 mantissa exactly.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a 24-bit integer is exact in f32"
        )]
        let value = (self.next() >> 40) as f32;
        value / 16_777_216.0
    }

    /// -1..1.
    fn signed(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }

    /// 0..n.
    fn below(&mut self, n: i32) -> i32 {
        let n = u64::try_from(n.max(1)).unwrap_or(1);
        i32::try_from(self.next() % n).unwrap_or(0)
    }
}

/// Back in the window after a while: the certificate, Cider's settings and
/// the session may have changed outside the app (a certificate tool, Cider's
/// own settings), so read them again; the issue line follows at once.
fn recheck_outside_state(ctx: &Rc<AppContext>) {
    let due = ctx
        .overview
        .focus_checked
        .get()
        .is_none_or(|at| at.elapsed() >= FOCUS_RECHECK);
    if !due || !ctx.overview.first_start_done.get() {
        return;
    }
    ctx.overview.focus_checked.set(Some(Instant::now()));
    check_ca_quietly(ctx);
    super::cider_page::refresh_quietly(ctx);
    check_session(ctx);
}

/// The user flipped the switch (Overview or tray): switches between Active
/// and Passthrough everywhere, and from now on the mode is theirs.
pub fn set_active(ctx: &Rc<AppContext>, active: bool) {
    ctx.overview.policy.set(ModePolicy::User);
    switch_mode(ctx, active);
}

/// Switches between Active and Passthrough everywhere (UI, proxy, config, tray).
fn switch_mode(ctx: &Rc<AppContext>, active: bool) {
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
            // The first start that worked (an earlier one may have failed,
            // say on a port in use) decides the mode and reads the rest.
            if !restarted || !ctx.overview.first_start_done.get() {
                ctx.overview.first_start_done.set(true);
                check_ca(ctx);
                check_session(ctx);
                super::library::reload(ctx);
                super::cider_page::refresh(ctx);
            }
        }
        ServiceEvent::Failed(error) => {
            ctx.with_ui(|ui| {
                let state = ui.global::<ProxyState>();
                state.set_starting(false);
                state.set_running(false);
                state.set_last_error(
                    format::breakable(&format!("Could not start the proxy: {error}")).into(),
                );
            });
            super::settings_page::on_start_failed(ctx, &error);
            ctx.sync_tray();
        }
        ServiceEvent::Status(status) => apply_status(ctx, &status),
        // A sync may have replaced or dropped the tokens; a capture stored
        // new ones.
        ServiceEvent::LibrarySync(_) | ServiceEvent::SessionChanged => check_session(ctx),
        // Download events are split off in `main` into `UiEvent::Download`
        // and routed to the Downloads page; this arm is unreachable in
        // practice but keeps the match exhaustive.
        ServiceEvent::Download(_) | ServiceEvent::DownloadsResync => {}
    }
}

fn apply_status(ctx: &Rc<AppContext>, status: &ProxyStatus) {
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
            state.set_last_error(format::breakable(error).into());
        }
    });
    super::settings_page::on_listening(ctx, status.listening.port());
    // Traffic is flowing: Cider may just have sent its tokens.
    let due = ctx
        .overview
        .session_checked
        .get()
        .is_some_and(|at| at.elapsed() >= SESSION_RECHECK);
    if due {
        check_session(ctx);
    }
    ctx.sync_tray();
}

/// Looks up whether Cider's Apple Music tokens were captured
/// (`ProxyState.session-captured`). Token values never leave the store.
fn check_session(ctx: &Rc<AppContext>) {
    if ctx.overview.session_checking.replace(true) {
        ctx.overview.session_again.set(true);
        return;
    }
    ctx.with_backend(
        |backend| async move {
            let developer = backend
                .storage
                .get_auth_token(TokenKind::DeveloperToken)
                .await;
            let user = backend
                .storage
                .get_auth_token(TokenKind::MediaUserToken)
                .await;
            match (developer, user) {
                (Ok(developer), Ok(user)) => Ok(developer.is_some() && user.is_some()),
                (Err(error), _) | (_, Err(error)) => Err(error.to_string()),
            }
        },
        |ctx, result| {
            ctx.overview.session_checking.set(false);
            let captured = match result {
                Some(Ok(captured)) => captured,
                Some(Err(error)) => {
                    tracing::warn!(%error, "could not look up the Apple Music session");
                    true
                }
                None => true,
            };
            // Look again on later traffic until it is there.
            ctx.overview
                .session_checked
                .set((!captured).then(Instant::now));
            ctx.with_ui(|ui| ui.global::<ProxyState>().set_session_captured(captured));
            if ctx.overview.session_again.replace(false) {
                check_session(ctx);
            }
        },
    );
}

/// Re-checks whether the CA is trusted.
pub fn check_ca(ctx: &Rc<AppContext>) {
    ctx.with_ui(|ui| ui.global::<ProxyState>().set_trust_state("checking".into()));
    check_ca_quietly(ctx);
}

/// [`check_ca`] without showing "Checking…" first (a background re-check
/// must not make the issue line or the Settings row flicker).
fn check_ca_quietly(ctx: &Rc<AppContext>) {
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
                    tracing::info!("certificate installed");
                    show_trust(ctx, &state, None);
                }
                Some(Err(error)) => {
                    tracing::warn!(%error, "certificate install failed");
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
            tracing::info!(message, "certificate uninstall");
            super::settings_page::show_message(ctx, &message);
        },
    );
}

fn set_trust_busy(ctx: &AppContext, busy: bool) {
    ctx.with_ui(|ui| ui.global::<ProxyState>().set_trust_busy(busy));
}

fn show_trust(ctx: &Rc<AppContext>, state: &TrustState, error: Option<&str>) {
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
        ps.set_trust_message(format::breakable(&message).into());
    });
    follow_trust(ctx, state);
}

/// Applies [`mode_after_trust_check`]: the first answer decides the startup
/// mode, later ones keep the automatic mode in step with the certificate.
fn follow_trust(ctx: &Rc<AppContext>, state: &TrustState) {
    let trusted = match state {
        TrustState::Trusted { .. } => Some(true),
        TrustState::Untrusted => Some(false),
        TrustState::Unknown(_) => None,
    };
    let policy = ctx.overview.policy.get();
    let next = mode_after_trust_check(policy, trusted);
    if policy == ModePolicy::Pending {
        ctx.overview.policy.set(ModePolicy::Automatic);
    }
    let Some(mode) = next else { return };
    let active = mode == Mode::Active;
    let current = ctx
        .ui
        .upgrade()
        .is_some_and(|ui| ui.global::<ProxyState>().get_active());
    if current != active || policy == ModePolicy::Pending {
        tracing::info!(%mode, ?trusted, "mode follows the certificate");
        switch_mode(ctx, active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_mode_should_be_passthrough_before_the_first_trust_check() {
        assert_eq!(
            [Mode::Active, Mode::Passthrough].map(|m| start_mode(ModePolicy::Pending, m)),
            [Mode::Passthrough, Mode::Passthrough]
        );
    }

    #[test]
    fn start_mode_should_keep_the_mode_once_decided() {
        assert_eq!(
            [ModePolicy::Automatic, ModePolicy::User].map(|p| start_mode(p, Mode::Active)),
            [Mode::Active, Mode::Active]
        );
    }

    #[test]
    fn note_seeds_should_fill_the_grid() {
        assert_eq!(
            note_seeds().len(),
            usize::from(NOTE_COLUMNS) * usize::from(NOTE_ROWS)
        );
    }

    #[test]
    fn note_seeds_should_stay_on_the_page() {
        assert!(
            note_seeds()
                .iter()
                .all(|n| (0.0..=1.0).contains(&n.x) && (0.0..=1.0).contains(&n.y))
        );
    }

    #[test]
    fn note_seeds_should_use_every_glyph_index_in_range() {
        assert!(note_seeds().iter().all(|n| (0..4).contains(&n.glyph)));
    }

    #[test]
    fn note_seeds_should_be_the_same_every_start() {
        assert_eq!(note_seeds(), note_seeds());
    }

    #[test]
    fn startup_should_be_active_when_the_certificate_is_trusted() {
        assert_eq!(
            mode_after_trust_check(ModePolicy::Pending, Some(true)),
            Some(Mode::Active)
        );
    }

    #[test]
    fn startup_should_stay_in_passthrough_while_the_certificate_is_untrusted() {
        assert_eq!(
            mode_after_trust_check(ModePolicy::Pending, Some(false)),
            Some(Mode::Passthrough)
        );
    }

    #[test]
    fn startup_should_stay_in_passthrough_when_trust_cannot_be_read() {
        assert_eq!(
            mode_after_trust_check(ModePolicy::Pending, None),
            Some(Mode::Passthrough)
        );
    }

    #[test]
    fn automatic_mode_should_turn_active_once_the_certificate_is_trusted() {
        assert_eq!(
            mode_after_trust_check(ModePolicy::Automatic, Some(true)),
            Some(Mode::Active)
        );
    }

    #[test]
    fn automatic_mode_should_fall_back_to_passthrough_when_the_certificate_goes() {
        assert_eq!(
            mode_after_trust_check(ModePolicy::Automatic, Some(false)),
            Some(Mode::Passthrough)
        );
    }

    #[test]
    fn automatic_mode_should_keep_its_mode_when_trust_cannot_be_read() {
        assert_eq!(mode_after_trust_check(ModePolicy::Automatic, None), None);
    }

    #[test]
    fn a_mode_the_user_chose_should_never_be_changed() {
        assert_eq!(
            [Some(true), Some(false), None].map(|t| mode_after_trust_check(ModePolicy::User, t)),
            [None, None, None]
        );
    }
}
