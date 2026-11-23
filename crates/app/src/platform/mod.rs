//! Small per-OS calls with no portable equivalent.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Lets process `pid` bring its window to the foreground.
///
/// A secondary launch calls this before asking the primary to show itself,
/// because the OS only lets the app the user is interacting with take focus.
/// Windows: `AllowSetForegroundWindow`. macOS 14+: `yieldActivationToApplication`.
/// Elsewhere a no-op (Linux passes an activation token instead).
pub fn allow_foreground(pid: u32) {
    #[cfg(windows)]
    windows::allow_foreground(pid);
    #[cfg(target_os = "macos")]
    macos::allow_foreground(pid);
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = pid;
}

/// Shows (`true`) or hides (`false`) the app's Dock icon.
///
/// macOS: activation policy `Regular` (Dock icon, app menu) or `Accessory`
/// (menu-bar item only, while the window is hidden to the tray). Elsewhere a
/// no-op: the taskbar entry follows the window.
pub fn set_dock_visible(visible: bool) {
    #[cfg(target_os = "macos")]
    macos::set_dock_visible(visible);
    #[cfg(not(target_os = "macos"))]
    let _ = visible;
}

/// Makes this app the active one so its window comes to the front.
///
/// macOS: `activate` (14+, cooperative) or `activateIgnoringOtherApps` (13).
/// Elsewhere a no-op: focusing the window is enough.
pub fn activate_app() {
    #[cfg(target_os = "macos")]
    macos::activate_app();
}

/// Whether the OS asks apps to reduce motion.
///
/// Windows: Settings > Accessibility > Visual effects > Animation effects
/// (`UISettings.AnimationsEnabled`). macOS: Accessibility > Display > Reduce
/// motion. Linux: GNOME's "enable-animations" through the desktop portal.
/// `false` when it can't be read.
pub fn prefers_reduced_motion() -> bool {
    #[cfg(windows)]
    let reduced = windows::prefers_reduced_motion();
    #[cfg(target_os = "macos")]
    let reduced = macos::prefers_reduced_motion();
    #[cfg(target_os = "linux")]
    let reduced = linux::prefers_reduced_motion();
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    let reduced = false;
    reduced
}
