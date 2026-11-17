//! Windows: the current user's `Root` certificate store.
//!
//! Adding a certificate to `CurrentUser\Root` makes Windows show its own
//! security warning dialog, which is the user's consent. No administrator
//! rights are needed, and Chromium-based apps (Cider) trust this store.

use std::io;

use schannel::cert_context::CertContext;
use schannel::cert_store::{CertAdd, CertStore};

use crate::{CA_COMMON_NAME, CertificateAuthority, TrustError};

/// System store name for trusted root certification authorities.
const ROOT_STORE: &str = "Root";
/// `ERROR_CANCELLED`: the user declined the security warning dialog.
const ERROR_CANCELLED: i32 = 1223;

pub(super) fn is_installed(ca: &CertificateAuthority) -> Result<bool, TrustError> {
    let store = open_root()?;
    Ok(matching(&store, ca).next().is_some())
}

pub(super) fn install(ca: &CertificateAuthority) -> Result<(), TrustError> {
    let mut store = open_root()?;
    if matching(&store, ca).next().is_some() {
        return Ok(());
    }
    let cert = CertContext::new(ca.cert_der()).map_err(TrustError::Store)?;
    let added = store
        .add_cert(&cert, CertAdd::ReplaceExisting)
        .map_err(map_store_error)?;
    // Cosmetic only: shows a readable name in certmgr.msc.
    if let Err(err) = added.set_friendly_name(CA_COMMON_NAME) {
        tracing::debug!(%err, "could not set certificate friendly name");
    }
    Ok(())
}

pub(super) fn uninstall(ca: &CertificateAuthority) -> Result<(), TrustError> {
    let store = open_root()?;
    // Collect first: deleting while the store enumeration is live is not
    // documented as safe.
    let found: Vec<CertContext> = matching(&store, ca).collect();
    for cert in found {
        // Deleting from the user Root store also shows a confirmation dialog.
        cert.delete().map_err(map_store_error)?;
    }
    Ok(())
}

fn open_root() -> Result<CertStore, TrustError> {
    CertStore::open_current_user(ROOT_STORE).map_err(TrustError::Store)
}

fn matching<'a>(
    store: &'a CertStore,
    ca: &'a CertificateAuthority,
) -> impl Iterator<Item = CertContext> + 'a {
    store
        .certs()
        .filter(move |cert| cert.to_der() == ca.cert_der())
}

fn map_store_error(err: io::Error) -> TrustError {
    if err.raw_os_error() == Some(ERROR_CANCELLED) {
        TrustError::Cancelled
    } else {
        TrustError::Store(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_store_error_should_report_cancelled_for_error_cancelled() {
        let err = map_store_error(io::Error::from_raw_os_error(ERROR_CANCELLED));
        assert!(matches!(err, TrustError::Cancelled));
    }

    #[test]
    fn map_store_error_should_keep_other_os_errors() {
        let err = map_store_error(io::Error::from_raw_os_error(5));
        assert!(matches!(err, TrustError::Store(_)));
    }

    #[test]
    fn is_installed_should_be_false_for_fresh_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = CertificateAuthority::load_or_create(dir.path()).unwrap();
        assert!(!is_installed(&ca).unwrap());
    }
}
