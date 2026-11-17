//! Local root certificate authority management for Cider Offline.
//!
//! - [`CertificateAuthority`] generates (once) and loads the proxy's root CA,
//!   persisted as PEM files in a caller-supplied directory.
//! - [`TrustStore`] is the OS trust-store abstraction used to install or
//!   remove that CA. [`system_trust_store`] returns the implementation for the
//!   current OS: the current-user `Root` store on Windows, the login keychain
//!   with user trust settings on macOS, and the NSS user database plus the
//!   system anchors on Linux. Other platforms report
//!   [`TrustError::NotImplemented`].

mod ca;
mod error;
mod store;

pub use ca::{CA_CERT_FILE, CA_COMMON_NAME, CA_KEY_FILE, CertificateAuthority};
pub use error::TrustError;
pub use store::{SystemTrustStore, TrustStatus, TrustStore, system_trust_store};

/// Re-export of the `rcgen` crate so downstream crates use the exact same
/// version for [`rcgen::Issuer`] / [`rcgen::KeyPair`].
pub use rcgen;
