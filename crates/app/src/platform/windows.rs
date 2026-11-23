//! Windows calls.
//!
//! This module holds the one scoped OS-call `unsafe` exception allowed by the
//! spec: `AllowSetForegroundWindow`. The `UISettings` call below is safe.

use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// Lets process `pid` take the foreground. The secondary launch is the app
/// the user just started, so it may pass that right on.
pub fn allow_foreground(pid: u32) {
    // SAFETY: `AllowSetForegroundWindow` takes a plain process id by value,
    // reads no memory of ours and has no preconditions; an unknown pid only
    // makes it return FALSE.
    #[allow(unsafe_code)]
    let allowed = unsafe { AllowSetForegroundWindow(pid) };
    if allowed == 0 {
        tracing::debug!(
            error = %std::io::Error::last_os_error(),
            pid,
            "AllowSetForegroundWindow failed"
        );
    }
}

/// Whether "Animation effects" is off (Settings > Accessibility > Visual
/// effects).
pub fn prefers_reduced_motion() -> bool {
    match ::windows::UI::ViewManagement::UISettings::new()
        .and_then(|settings| settings.AnimationsEnabled())
    {
        Ok(enabled) => !enabled,
        Err(error) => {
            tracing::debug!(%error, "could not read the animation setting");
            false
        }
    }
}
