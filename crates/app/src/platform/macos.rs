//! macOS calls (all safe objc2 bindings).

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSRunningApplication};

/// Yields activation to process `pid` (macOS 14+, where cooperative
/// activation applies). Must run on the main thread; otherwise a no-op.
pub fn allow_foreground(pid: u32) {
    if !objc2::available!(macos = 14.0) {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::debug!("allow_foreground called off the main thread");
        return;
    };
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        tracing::debug!(pid, "no running application for pid");
        return;
    };
    NSApplication::sharedApplication(mtm).yieldActivationToApplication(&app);
}

/// Switches between `Regular` (Dock icon) and `Accessory` (menu bar only).
/// Must run on the main thread; otherwise a no-op.
pub fn set_dock_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::debug!("set_dock_visible called off the main thread");
        return;
    };
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    if !NSApplication::sharedApplication(mtm).setActivationPolicy(policy) {
        tracing::debug!(visible, "could not change the activation policy");
    }
}

/// Activates the app. Must run on the main thread; otherwise a no-op.
pub fn activate_app() {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::debug!("activate_app called off the main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    if objc2::available!(macos = 14.0) {
        // Cooperative: succeeds when the second launch yielded to us
        // (`allow_foreground`) or the user clicked our menu-bar item.
        app.activate();
    } else {
        #[expect(
            deprecated,
            reason = "`activate` needs macOS 14; this is the macOS 13 fallback"
        )]
        app.activateIgnoringOtherApps(true);
    }
}
