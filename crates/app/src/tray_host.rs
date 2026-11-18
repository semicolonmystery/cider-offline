//! Whether a system tray host exists, and what to do when that changes.
//!
//! On Linux the tray is a `StatusNotifierItem`, which needs a
//! `StatusNotifierWatcher` on the session bus (KDE, or GNOME with the
//! `AppIndicator` extension). Without one the icon is invisible, so the app
//! must behave as if it had no tray: closing quits, and a hidden window is
//! brought back if the host vanishes. Other OSes always have a tray.

/// The well-known bus name of the `StatusNotifierItem` host.
#[cfg(target_os = "linux")]
pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";

/// Reports whether a tray host exists now and calls `on_change` on a
/// background thread whenever one appears or vanishes.
///
/// Linux: checks `name_has_owner` once and watches `NameOwnerChanged` for
/// [`WATCHER`]; without a session bus the answer is `false` and no thread
/// runs. Other OSes: `true`, no thread.
pub fn watch(on_change: impl Fn(bool) + Send + 'static) -> bool {
    imp::watch(on_change)
}

#[cfg(target_os = "linux")]
mod imp {
    use zbus::blocking::{Connection, fdo::DBusProxy};
    use zbus::names::BusName;

    use super::WATCHER;

    pub fn watch(on_change: impl Fn(bool) + Send + 'static) -> bool {
        let (connection, proxy) = match Connection::session().and_then(|c| {
            let proxy = DBusProxy::new(&c)?;
            Ok((c, proxy))
        }) {
            Ok(pair) => pair,
            Err(error) => {
                tracing::info!(%error, "no session bus; assuming no tray host");
                return false;
            }
        };
        // Subscribe before the initial query so a change in between is not lost.
        let changes = match proxy.receive_name_owner_changed_with_args(&[(0, WATCHER)]) {
            Ok(changes) => changes,
            Err(error) => {
                tracing::warn!(%error, "cannot watch for the tray host");
                return false;
            }
        };
        let present = BusName::try_from(WATCHER)
            .ok()
            .and_then(|name| proxy.name_has_owner(name).ok())
            .unwrap_or(false);
        let spawned = std::thread::Builder::new()
            .name("tray-host-watch".into())
            .spawn(move || {
                // Keeps the connection alive for the lifetime of the iterator.
                let _connection = connection;
                for signal in changes {
                    match signal.args() {
                        Ok(args) => on_change(args.new_owner().is_some()),
                        Err(error) => tracing::debug!(%error, "bad NameOwnerChanged signal"),
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the tray host watcher");
        }
        present
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn watch(_on_change: impl Fn(bool) + Send + 'static) -> bool {
        true
    }
}

/// How the window starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// Shown normally.
    Visible,
    /// Not shown at all; the tray is the way back in.
    Hidden,
    /// Shown, then minimized (start-minimized without a tray).
    Minimized,
}

/// Start mode for `start_hidden` (config or `--minimized`) and tray state.
pub fn start_mode(start_hidden: bool, tray_available: bool) -> StartMode {
    match (start_hidden, tray_available) {
        (true, true) => StartMode::Hidden,
        (true, false) => StartMode::Minimized,
        (false, _) => StartMode::Visible,
    }
}

/// What to do when the tray host appears or vanishes.
#[derive(Debug, PartialEq, Eq)]
pub enum HostChange {
    /// Bring the window back.
    ShowWindow,
    /// Nothing to do.
    Nothing,
}

/// A vanished tray host while the window is hidden would lose the app.
pub fn on_host_change(available: bool, window_visible: bool) -> HostChange {
    if !available && !window_visible {
        HostChange::ShowWindow
    } else {
        HostChange::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_mode_should_hide_only_with_tray() {
        assert_eq!(start_mode(true, true), StartMode::Hidden);
        assert_eq!(start_mode(true, false), StartMode::Minimized);
        assert_eq!(start_mode(false, false), StartMode::Visible);
    }

    #[test]
    fn on_host_change_should_show_hidden_window_when_tray_vanishes() {
        assert_eq!(on_host_change(false, false), HostChange::ShowWindow);
        assert_eq!(on_host_change(false, true), HostChange::Nothing);
        assert_eq!(on_host_change(true, false), HostChange::Nothing);
    }
}
