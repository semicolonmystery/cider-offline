//! Linux desktop settings, read through the XDG desktop portal.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedValue;

/// How long an answer is reused: the call opens a session-bus connection.
const REUSE: Duration = Duration::from_secs(30);

static CACHED: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// GNOME's "Animations" switch (`org.gnome.desktop.interface
/// enable-animations`), which the portal's GNOME and GTK backends expose.
/// `false` without a portal or the key (KDE and others).
pub fn prefers_reduced_motion() -> bool {
    let mut cached = CACHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, reduced)) = *cached
        && at.elapsed() < REUSE
    {
        return reduced;
    }
    let reduced = match read_enable_animations() {
        Ok(enabled) => !enabled,
        Err(error) => {
            tracing::debug!(%error, "could not read the animation setting");
            false
        }
    };
    *cached = Some((Instant::now(), reduced));
    reduced
}

fn read_enable_animations() -> zbus::Result<bool> {
    let connection = Connection::session()?;
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )?;
    let value: OwnedValue = proxy.call(
        "ReadOne",
        &("org.gnome.desktop.interface", "enable-animations"),
    )?;
    Ok(bool::try_from(value)?)
}
