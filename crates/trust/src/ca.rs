use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use sha1::{Digest, Sha1};
use time::{Duration, OffsetDateTime};

use crate::TrustError;

/// File name of the PEM-encoded CA certificate inside the CA directory.
pub const CA_CERT_FILE: &str = "ca.cert.pem";
/// File name of the PEM-encoded CA private key inside the CA directory.
pub const CA_KEY_FILE: &str = "ca.key.pem";

/// Subject common name of a newly generated CA certificate. Also used as the
/// display name / nickname when the CA is added to an OS trust store, and by
/// the packaging uninstall hooks.
pub const CA_COMMON_NAME: &str = "Cider Offline CA";
/// Every common name / trust-store nickname this app has used: the current
/// [`CA_COMMON_NAME`] first, then the one from before the rename to "Cider
/// Offline". An existing CA keeps its original subject, so stores that look
/// the CA up by name (NSS, the macOS keychain) try all of them.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) const KNOWN_CA_COMMON_NAMES: &[&str] = &[CA_COMMON_NAME, "Cider Offline Proxy Local CA"];
const ORGANIZATION: &str = "Cider Offline";
/// Roughly ten years.
const VALIDITY_DAYS: i64 = 3650;

/// The proxy's local root certificate authority.
///
/// Holds the PEM text of the certificate and key. The key never leaves the
/// local machine; it is only used to sign per-host leaf certificates.
pub struct CertificateAuthority {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key_pem: String,
    cert_path: PathBuf,
}

impl std::fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the private key.
        f.debug_struct("CertificateAuthority")
            .field("cert_path", &self.cert_path)
            .finish_non_exhaustive()
    }
}

impl CertificateAuthority {
    /// Loads the CA from `dir`, generating and persisting a new one when
    /// neither file exists yet.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError::Incomplete`] if only one of the two files is
    /// present, and I/O or certificate errors otherwise.
    pub fn load_or_create(dir: &Path) -> Result<Self, TrustError> {
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);

        match (cert_path.is_file(), key_path.is_file()) {
            (true, true) => Self::load(cert_path, &key_path),
            (false, false) => Self::create(dir, cert_path, &key_path),
            _ => Err(TrustError::Incomplete {
                dir: dir.to_path_buf(),
            }),
        }
    }

    /// Loads the CA from `dir` without ever creating one.
    ///
    /// Returns `Ok(None)` when neither file exists.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError::Incomplete`] if only one of the two files is
    /// present, and I/O or certificate errors otherwise.
    pub fn load_existing(dir: &Path) -> Result<Option<Self>, TrustError> {
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);
        match (cert_path.is_file(), key_path.is_file()) {
            (true, true) => Self::load(cert_path, &key_path).map(Some),
            (false, false) => Ok(None),
            _ => Err(TrustError::Incomplete {
                dir: dir.to_path_buf(),
            }),
        }
    }

    fn load(cert_path: PathBuf, key_path: &Path) -> Result<Self, TrustError> {
        let cert_pem = fs::read_to_string(&cert_path).map_err(|e| TrustError::io(&cert_path, e))?;
        let key_pem = fs::read_to_string(key_path).map_err(|e| TrustError::io(key_path, e))?;
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())?;
        let ca = Self {
            cert_pem,
            cert_der,
            key_pem,
            cert_path,
        };
        // Validate eagerly so a corrupt CA fails at startup, not on the first handshake.
        ca.issuer()?;
        tracing::debug!(path = %ca.cert_path.display(), "loaded local CA");
        Ok(ca)
    }

    fn create(dir: &Path, cert_path: PathBuf, key_path: &Path) -> Result<Self, TrustError> {
        fs::create_dir_all(dir).map_err(|e| TrustError::io(dir, e))?;

        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(DnType::CommonName, CA_COMMON_NAME);
        params
            .distinguished_name
            .push(DnType::OrganizationName, ORGANIZATION);
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(VALIDITY_DAYS);

        let key_pair = KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let cert_der = cert.der().clone();
        let key_pem = key_pair.serialize_pem();

        write_private(key_path, key_pem.as_bytes())?;
        fs::write(&cert_path, cert_pem.as_bytes()).map_err(|e| TrustError::io(&cert_path, e))?;
        tracing::info!(path = %cert_path.display(), "generated new local CA");

        Ok(Self {
            cert_pem,
            cert_der,
            key_pem,
            cert_path,
        })
    }

    /// PEM text of the CA certificate (public; safe to export).
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// DER encoding of the CA certificate.
    pub fn cert_der(&self) -> &[u8] {
        self.cert_der.as_ref()
    }

    /// SHA-1 fingerprint of the DER certificate (the "thumbprint" that
    /// Windows, macOS and NSS tools display and accept).
    pub fn sha1_fingerprint(&self) -> [u8; 20] {
        Sha1::digest(self.cert_der()).into()
    }

    /// [`Self::sha1_fingerprint`] as uppercase hex without separators.
    pub fn sha1_fingerprint_hex(&self) -> String {
        use std::fmt::Write as _;
        self.sha1_fingerprint()
            .iter()
            .fold(String::with_capacity(40), |mut hex, byte| {
                // Writing to a String cannot fail.
                let _ = write!(hex, "{byte:02X}");
                hex
            })
    }

    /// Path of the persisted CA certificate file.
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// Builds a signing [`Issuer`] for leaf certificates, as expected by
    /// `hudsucker::certificate_authority::RcgenAuthority::new`.
    ///
    /// # Errors
    ///
    /// Returns a certificate error if the stored PEM cannot be parsed.
    pub fn issuer(&self) -> Result<Issuer<'static, KeyPair>, TrustError> {
        let key = KeyPair::from_pem(&self.key_pem)?;
        Ok(Issuer::from_ca_cert_pem(&self.cert_pem, key)?)
    }
}

/// Writes a secret file, restricting permissions to the owner where supported.
///
/// On Windows the file inherits the ACL of the per-user data directory, which
/// is already private to the user.
fn write_private(path: &Path, contents: &[u8]) -> Result<(), TrustError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| TrustError::io(path, e))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|e| TrustError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_common_names_should_start_with_current_name() {
        assert_eq!(KNOWN_CA_COMMON_NAMES.first(), Some(&CA_COMMON_NAME));
    }

    #[test]
    fn known_common_names_should_keep_pre_rename_name() {
        assert!(KNOWN_CA_COMMON_NAMES.contains(&"Cider Offline Proxy Local CA"));
    }

    #[test]
    fn load_or_create_should_persist_and_reload_same_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let created = CertificateAuthority::load_or_create(dir.path()).unwrap();
        let loaded = CertificateAuthority::load_or_create(dir.path()).unwrap();
        assert_eq!(created.cert_pem(), loaded.cert_pem());
    }

    #[test]
    fn load_existing_should_return_none_without_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            CertificateAuthority::load_existing(dir.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn load_existing_should_not_create_files() {
        let dir = tempfile::tempdir().unwrap();
        CertificateAuthority::load_existing(dir.path()).unwrap();
        assert!(!dir.path().join(CA_CERT_FILE).exists());
    }

    #[test]
    fn load_existing_should_load_created_ca() {
        let dir = tempfile::tempdir().unwrap();
        let created = CertificateAuthority::load_or_create(dir.path()).unwrap();
        let loaded = CertificateAuthority::load_existing(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(created.cert_pem(), loaded.cert_pem());
    }

    #[test]
    fn load_or_create_should_produce_usable_issuer() {
        let dir = tempfile::tempdir().unwrap();
        let ca = CertificateAuthority::load_or_create(dir.path()).unwrap();
        assert!(ca.issuer().is_ok());
    }

    #[test]
    fn load_or_create_should_refuse_when_key_missing() {
        let dir = tempfile::tempdir().unwrap();
        CertificateAuthority::load_or_create(dir.path()).unwrap();
        fs::remove_file(dir.path().join(CA_KEY_FILE)).unwrap();
        let err = CertificateAuthority::load_or_create(dir.path()).unwrap_err();
        assert!(matches!(err, TrustError::Incomplete { .. }));
    }

    #[test]
    fn cert_der_should_survive_pem_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let created = CertificateAuthority::load_or_create(dir.path()).unwrap();
        let loaded = CertificateAuthority::load_or_create(dir.path()).unwrap();
        assert_eq!(created.cert_der(), loaded.cert_der());
    }

    #[test]
    fn sha1_fingerprint_hex_should_be_40_uppercase_hex_digits() {
        let dir = tempfile::tempdir().unwrap();
        let ca = CertificateAuthority::load_or_create(dir.path()).unwrap();
        let hex = ca.sha1_fingerprint_hex();
        assert!(
            hex.len() == 40 && hex.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')),
            "unexpected fingerprint {hex}"
        );
    }
}
