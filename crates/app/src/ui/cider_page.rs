//! Overview "Route Cider through this proxy" control.
//!
//! Reads Cider's live proxy state from [`crate::cider`] and lets the user
//! toggle it. All file and process work runs on the tokio runtime via
//! [`AppContext::spawn`] (wrapping the blocking calls in `spawn_blocking`);
//! results are pushed back to the UI thread. Where [`cider::SUPPORT`] is
//! `GuideOnly` the card shows the manual steps and no switch; otherwise the
//! control is disabled, with a short explanation, whenever Cider's config
//! cannot be found, and the user can pick the file by hand.

use std::path::PathBuf;
use std::rc::Rc;

use slint::ComponentHandle as _;

use super::{AppContext, CiderState, ProxyState, dialogs};
use crate::cider;

/// Wires the Cider control callbacks and reads the initial state.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<CiderState>();

    let c = Rc::clone(ctx);
    state.on_set_routed(move |routed| set_routed(&c, routed));
    let c = Rc::clone(ctx);
    state.on_refresh(move || refresh(&c));
    let c = Rc::clone(ctx);
    state.on_choose_config(move || choose_config(&c));

    state.set_automatic(cider::SUPPORT == cider::Support::Automatic);

    refresh(ctx);
}

/// Re-reads Cider's current proxy state and updates the control.
pub fn refresh(ctx: &Rc<AppContext>) {
    read_state(ctx, true);
}

/// [`refresh`] in the background (the window regained focus): the switch is
/// not disabled while the file is read, so nothing flickers.
pub fn refresh_quietly(ctx: &Rc<AppContext>) {
    read_state(ctx, false);
}

fn read_state(ctx: &Rc<AppContext>, show_busy: bool) {
    // The guide names this proxy's configured listen address, which can
    // change between refreshes.
    let guide = cider::manual_guide(&ctx.config().listen_addr().to_string());
    ctx.with_ui(|ui| ui.global::<CiderState>().set_guide(guide.into()));
    if cider::SUPPORT != cider::Support::Automatic {
        ctx.with_ui(|ui| {
            let cs = ui.global::<CiderState>();
            cs.set_available(false);
            cs.set_routed(false);
        });
        return;
    }
    if show_busy {
        set_busy(ctx, true);
    }
    let server = current_server(ctx);
    let chosen = chosen_config(ctx);
    ctx.spawn(
        async move {
            tokio::task::spawn_blocking(move || {
                let path = cider::locate(chosen.as_deref())?;
                let state = cider::read_state(&path, &server)?;
                Ok::<_, cider::CiderError>((path, state))
            })
            .await
        },
        move |ctx, result| {
            if show_busy {
                set_busy(ctx, false);
            }
            match result {
                Ok(Ok((path, state))) => {
                    let routed = state.use_proxy && state.points_at_us;
                    ctx.with_ui(|ui| {
                        let cs = ui.global::<CiderState>();
                        cs.set_available(true);
                        cs.set_routed(routed);
                        cs.set_config_path(path.display().to_string().into());
                    });
                }
                Ok(Err(error)) => {
                    ctx.with_ui(|ui| {
                        let cs = ui.global::<CiderState>();
                        cs.set_available(false);
                        cs.set_routed(false);
                        cs.set_config_path("".into());
                    });
                    set_message(ctx, &error.to_string());
                }
                Err(error) => {
                    set_message(ctx, &format!("Could not read Cider's settings: {error}"));
                }
            }
        },
    );
}

/// Lets the user pick Cider's config file, remembers it and re-reads the state.
fn choose_config(ctx: &Rc<AppContext>) {
    let c = Rc::clone(ctx);
    dialogs::pick_file(
        ctx,
        "Choose Cider's config file",
        "Cider config",
        &["yml", "yaml"],
        move |picked| {
            if let Some(path) = picked {
                c.update_config(|config| config.cider_config_path = Some(path));
                set_message(&c, "");
                refresh(&c);
            }
        },
    );
}

/// The config file the user picked earlier, if any.
fn chosen_config(ctx: &AppContext) -> Option<PathBuf> {
    ctx.config().cider_config_path.clone()
}

/// Handles the user flipping the switch.
fn set_routed(ctx: &Rc<AppContext>, enabled: bool) {
    set_busy(ctx, true);
    let server = current_server(ctx);
    ctx.spawn(
        async move {
            tokio::task::spawn_blocking(cider::is_cider_running)
                .await
                .unwrap_or(false)
        },
        move |ctx, running| {
            if running {
                let c = Rc::clone(ctx);
                dialogs::ask(
                    ctx,
                    dialogs::Prompt {
                        title: "Restart Cider?",
                        message: "Cider must restart to apply this. Close and reopen Cider now?",
                        confirm: "Restart",
                        destructive: false,
                    },
                    move |confirmed| {
                        if confirmed {
                            apply(&c, enabled, server);
                        } else {
                            // Declined: leave the config untouched and restore
                            // the switch to Cider's actual state.
                            set_message(&c, "No changes made.");
                            refresh(&c);
                        }
                    },
                );
            } else {
                apply(ctx, enabled, server);
            }
        },
    );
}

/// Stops Cider if needed, writes the config, and relaunches if it was running.
fn apply(ctx: &Rc<AppContext>, enabled: bool, server: String) {
    set_busy(ctx, true);
    let chosen = chosen_config(ctx);
    ctx.spawn(
        async move {
            tokio::task::spawn_blocking(move || {
                let path = cider::locate(chosen.as_deref())?;
                cider::apply_toggle(&path, enabled, &server)
            })
            .await
            .map_err(|error| format!("background task failed: {error}"))?
            .map_err(|error| error.to_string())
        },
        move |ctx, result: Result<cider::ApplyOutcome, String>| {
            set_busy(ctx, false);
            match result {
                Ok(outcome) => {
                    let message = if outcome.was_running {
                        "Cider restarted with the new proxy setting."
                    } else if outcome.enabled {
                        "Saved. Cider will use this proxy the next time it starts."
                    } else {
                        "Saved. Cider will start without this proxy next time."
                    };
                    set_message(ctx, message);
                    tracing::info!(message, "Cider proxy setting changed");
                }
                Err(error) => set_message(ctx, &format!("Could not update Cider: {error}")),
            }
            // Re-read so the switch reflects what was actually written.
            refresh(ctx);
        },
    );
}

/// This proxy's current listening address (the server Cider should point at).
fn current_server(ctx: &Rc<AppContext>) -> String {
    ctx.ui
        .upgrade()
        .map(|ui| ui.global::<ProxyState>().get_address().to_string())
        .filter(|address| !address.is_empty())
        .unwrap_or_else(|| ctx.config().listen_addr().to_string())
}

fn set_busy(ctx: &AppContext, busy: bool) {
    ctx.with_ui(|ui| ui.global::<CiderState>().set_busy(busy));
}

fn set_message(ctx: &AppContext, message: &str) {
    // Messages may name a path: let it wrap between folders.
    ctx.with_ui(|ui| {
        ui.global::<CiderState>()
            .set_message(super::format::breakable(message).into());
    });
}
