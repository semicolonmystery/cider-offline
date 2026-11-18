//! OS trust-store integration.
//!
//! | OS      | Store                                                   | Consent                         |
//! | ------- | ------------------------------------------------------- | ------------------------------- |
//! | Windows | Current-user `Root` store via the `schannel` crate    | Windows security warning dialog |
//! | macOS   | Login keychain + user-domain trust settings (`security`)| macOS authorization dialog      |
//! | Linux   | System anchors (`update-ca-*`) and the user NSS DB      | polkit (`pkexec`) prompt        |
//!
//! Chromium / Electron apps such as Cider use the platform store on Windows
//! and macOS, but the NSS database (`~/.pki/nssdb`) on Linux, which is why the
//! Linux implementation installs into both.

use crate::{CertificateAuthority, TrustError};

#[cfg(unix)]
mod command;
// Pure NSS path/aggregation logic; compiled on every OS under test so it is
// checked off-Linux too.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(test, target_os = "linux"))]
mod nss;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

/// Trust of the CA as seen by the app (Cider) and, optionally, system-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustStatus {
    /// Trusted where Cider looks (the NSS databases on Linux, the OS store
    /// elsewhere).
    pub app_trusted: bool,
    /// System-wide trust for other TLS clients. `None` when the OS has no such
    /// separate, optional layer.
    pub system_trusted: Option<bool>,
}

/// An operating-system certificate trust store.
///
/// Implementations may block (spawn OS tools, show consent or elevation
/// prompts), so async callers should run them via
/// `tokio::task::spawn_blocking`.
pub trait TrustStore {
    /// Returns whether `ca` is currently trusted by this store.
    fn is_installed(&self, ca: &CertificateAuthority) -> Result<bool, TrustError>;

    /// Reports trust split into what Cider uses and the optional system layer.
    ///
    /// The default reuses [`Self::is_installed`] for both.
    fn status(&self, ca: &CertificateAuthority) -> Result<TrustStatus, TrustError> {
        let installed = self.is_installed(ca)?;
        Ok(TrustStatus {
            app_trusted: installed,
            system_trusted: Some(installed),
        })
    }

    /// Adds `ca` to this store as a trusted root.
    ///
    /// Returns [`TrustError::Cancelled`] when the user declines the OS prompt.
    fn install(&self, ca: &CertificateAuthority) -> Result<(), TrustError>;

    /// Removes `ca` from this store. Removing a CA that is not installed is
    /// not an error.
    fn uninstall(&self, ca: &CertificateAuthority) -> Result<(), TrustError>;
}

/// The trust store of the current operating system (current user scope where
/// the OS offers one). See the module documentation for per-OS details.
///
/// On unsupported platforms every operation returns
/// [`TrustError::NotImplemented`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemTrustStore {
    _private: (),
}

/// Returns the trust store for the current operating system.
pub fn system_trust_store() -> SystemTrustStore {
    SystemTrustStore::default()
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl TrustStore for SystemTrustStore {
    fn is_installed(&self, ca: &CertificateAuthority) -> Result<bool, TrustError> {
        platform::is_installed(ca)
    }

    #[cfg(target_os = "linux")]
    fn status(&self, ca: &CertificateAuthority) -> Result<TrustStatus, TrustError> {
        platform::status(ca)
    }

    fn install(&self, ca: &CertificateAuthority) -> Result<(), TrustError> {
        platform::install(ca)?;
        tracing::info!(fingerprint = %ca.sha1_fingerprint_hex(), "installed local CA into OS trust store");
        Ok(())
    }

    fn uninstall(&self, ca: &CertificateAuthority) -> Result<(), TrustError> {
        platform::uninstall(ca)?;
        tracing::info!(fingerprint = %ca.sha1_fingerprint_hex(), "removed local CA from OS trust store");
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl TrustStore for SystemTrustStore {
    fn is_installed(&self, _ca: &CertificateAuthority) -> Result<bool, TrustError> {
        Err(TrustError::NotImplemented {
            operation: "trust store lookup",
        })
    }

    fn install(&self, _ca: &CertificateAuthority) -> Result<(), TrustError> {
        Err(TrustError::NotImplemented {
            operation: "trust store install",
        })
    }

    fn uninstall(&self, _ca: &CertificateAuthority) -> Result<(), TrustError> {
        Err(TrustError::NotImplemented {
            operation: "trust store uninstall",
        })
    }
}
