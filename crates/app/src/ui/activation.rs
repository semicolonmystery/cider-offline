//! Bringing the window forward when a second launch or the tray asks.
//!
//! The decision ([`raise_strategy`]) and the token hand-off
//! ([`PendingActivation`]) are pure so they are tested on every OS; the
//! window calls themselves live in `AppContext::show_window`.

use std::sync::{Arc, Mutex, PoisonError};

use slint::winit_030::winit;
use winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::window::{Window, WindowAttributes};

/// A desktop activation token (Linux `XDG_ACTIVATION_TOKEN` /
/// `DESKTOP_STARTUP_ID`) waiting for the next winit window creation.
///
/// Shared between `AppContext` (which stores the token of a second launch)
/// and the window-attributes hook installed in `main` (which takes it).
#[derive(Clone, Default)]
pub struct PendingActivation(Arc<Mutex<Option<String>>>);

impl PendingActivation {
    /// Stores `token`, replacing an older one that was never used.
    pub fn set(&self, token: String) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(token);
    }

    /// Stores `token` only when no winit window exists yet: the hook reads it
    /// only at window creation, so a token for an existing window would sit
    /// unused and be handed to an unrelated, later window.
    pub fn offer(&self, token: Option<String>, winit_window_exists: bool) {
        if let Some(token) = token.filter(|_| !winit_window_exists) {
            self.set(token);
        }
    }

    /// Takes the stored token, leaving none behind.
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Window-attributes hook: hands a pending token to the window being
    /// created so the compositor lets it take focus. A no-op off Linux.
    pub fn apply(&self, attributes: WindowAttributes) -> WindowAttributes {
        let Some(token) = self.take() else {
            return attributes;
        };
        #[cfg(target_os = "linux")]
        {
            use winit::platform::startup_notify::WindowAttributesExtStartupNotify as _;
            attributes.with_activation_token(winit::window::ActivationToken::from_raw(token))
        }
        // Only Linux launches pass tokens; elsewhere there is nothing to apply.
        #[cfg(not(target_os = "linux"))]
        {
            drop(token);
            attributes
        }
    }
}

/// How to bring the window forward.
#[derive(Debug, PartialEq, Eq)]
pub enum Raise {
    /// Un-minimize, show, activate the app and focus the window.
    ShowAndFocus,
    /// Only ask the desktop to flag the window.
    AttentionOnly,
}

/// Picks how to raise the window.
///
/// Wayland does not let a client focus its own, already mapped window
/// without a fresh token from the current input event, so a visible window
/// there only requests attention. Everywhere else (and for a hidden Wayland
/// window, which gets the token when it is created) show and focus it.
pub fn raise_strategy(is_wayland: bool, window_visible: bool) -> Raise {
    if is_wayland && window_visible {
        Raise::AttentionOnly
    } else {
        Raise::ShowAndFocus
    }
}

/// Whether `window` is a Wayland surface (as opposed to X11, Win32, `AppKit`).
pub fn is_wayland(window: &Window) -> bool {
    window
        .window_handle()
        .is_ok_and(|handle| matches!(handle.as_raw(), RawWindowHandle::Wayland(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_strategy_should_only_request_attention_for_visible_wayland_window() {
        assert_eq!(raise_strategy(true, true), Raise::AttentionOnly);
        assert_eq!(raise_strategy(true, false), Raise::ShowAndFocus);
        assert_eq!(raise_strategy(false, true), Raise::ShowAndFocus);
    }

    #[test]
    fn pending_activation_should_be_taken_once() {
        let pending = PendingActivation::default();
        pending.set("token-1".to_owned());
        assert_eq!(pending.take().as_deref(), Some("token-1"));
        assert_eq!(pending.take(), None);
    }

    #[test]
    fn offer_should_store_token_when_no_window_exists() {
        let pending = PendingActivation::default();
        pending.offer(Some("token-1".to_owned()), false);
        assert_eq!(pending.take().as_deref(), Some("token-1"));
    }

    #[test]
    fn offer_should_drop_token_when_window_exists() {
        let pending = PendingActivation::default();
        pending.offer(Some("token-1".to_owned()), true);
        assert_eq!(pending.take(), None);
    }
}
