//! OS trust-store operations for the proxy CA, run off the async workers.

use std::sync::Arc;

use cider_offline_trust::{
    CertificateAuthority, TrustError, TrustStatus, TrustStore as _, system_trust_store,
};

/// Trust state of the CA as shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// Cider trusts the CA. `system` is the optional system-wide trust, when
    /// the OS has such a separate layer.
    Trusted { system: Option<bool> },
    /// The CA is not trusted yet.
    Untrusted,
    /// The state could not be determined.
    Unknown(String),
}

impl TrustState {
    /// Identifier used by the Slint UI (`ProxyState.trust-state`).
    pub fn ui_key(&self) -> &'static str {
        match self {
            Self::Trusted { .. } => "trusted",
            Self::Untrusted => "untrusted",
            Self::Unknown(_) => "error",
        }
    }
}

/// Checks whether `ca` is trusted by the OS.
pub async fn check(ca: Arc<CertificateAuthority>) -> TrustState {
    match blocking(move || system_trust_store().status(&ca)).await {
        Ok(status) => from_status(status),
        Err(error) => TrustState::Unknown(error),
    }
}

fn from_status(status: TrustStatus) -> TrustState {
    if status.app_trusted {
        TrustState::Trusted {
            system: status.system_trusted,
        }
    } else {
        TrustState::Untrusted
    }
}

/// Installs `ca` into the OS trust store (may show an OS prompt).
pub async fn install(ca: Arc<CertificateAuthority>) -> Result<(), String> {
    blocking(move || system_trust_store().install(&ca)).await
}

/// Removes `ca` from the OS trust store (may show an OS prompt).
pub async fn uninstall(ca: Arc<CertificateAuthority>) -> Result<(), String> {
    blocking(move || system_trust_store().uninstall(&ca)).await
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, TrustError> + Send + 'static,
) -> Result<T, String> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(error) => Err(format!("trust store task failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_for_cider_only_should_show_as_trusted() {
        assert_eq!(
            TrustState::Trusted {
                system: Some(false)
            }
            .ui_key(),
            "trusted"
        );
    }

    #[test]
    fn from_status_should_be_untrusted_when_app_not_trusted() {
        let status = TrustStatus {
            app_trusted: false,
            system_trusted: Some(true),
        };
        assert_eq!(from_status(status), TrustState::Untrusted);
    }
}
