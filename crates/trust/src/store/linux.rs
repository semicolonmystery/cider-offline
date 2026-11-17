//! Linux: two stores. NSS is what Cider needs; the system anchor is optional.
//!
//! 1. **NSS user database** (`~/.pki/nssdb`, plus Flatpak Cider sandboxes
//!    under `~/.var/app/*cider*/.pki/nssdb` and Snap ones under
//!    `~/snap/*cider*/current/.pki/nssdb`). Chromium / Electron apps such as
//!    Cider read user-added roots from here, not from the system bundle.
//!    Managed with `certutil` (Debian/Ubuntu `libnss3-tools`, Fedora
//!    `nss-tools`, Arch `nss`). No elevation needed.
//! 2. **System anchors** (for every other TLS client), written via `pkexec`
//!    so the user gets the desktop's polkit authentication prompt. The anchor
//!    directory and refresh command are detected per distribution family.
//!    When no known layout exists this part is skipped with a warning.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::TrustStatus;
use super::command::Tool;
use super::nss::{combine, nss_database_candidates};
use crate::ca::KNOWN_CA_COMMON_NAMES;
use crate::{CA_COMMON_NAME, CertificateAuthority, TrustError};

const CERTUTIL: Tool = Tool {
    program: "certutil",
    missing_hint: "install the NSS tools package (Debian/Ubuntu: libnss3-tools, \
                   Fedora: nss-tools, Arch: nss, openSUSE: mozilla-nss-tools)",
};

const PKEXEC: Tool = Tool {
    program: "pkexec",
    missing_hint: "install polkit (package `pkexec` or `polkit`) to update the system trust store",
};

/// File stem used for the system anchor file.
const ANCHOR_STEM: &str = "cider-offline-ca";
/// Anchor file stem used before the rename to "Cider Offline"; still
/// recognised as installed and removed on uninstall.
const LEGACY_ANCHOR_STEM: &str = "cider-offline-proxy-ca";

/// Upper bound on NSS entries removed by `uninstall` (several can share a
/// nickname, and both the current and the legacy nickname may be present).
const MAX_NSS_DUPLICATES: usize = 16 * KNOWN_CA_COMMON_NAMES.len();

/// `pkexec` exit status when the user dismissed the authentication dialog.
const PKEXEC_DISMISSED: i32 = 126;

/// A distribution family's system trust-anchor layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SystemLayout {
    dir: &'static str,
    extension: &'static str,
    refresh: &'static [&'static str],
}

/// Known layouts, probed in order; the first existing directory wins.
const SYSTEM_LAYOUTS: &[SystemLayout] = &[
    // Fedora, RHEL, CentOS
    SystemLayout {
        dir: "/etc/pki/ca-trust/source/anchors",
        extension: "pem",
        refresh: &["update-ca-trust", "extract"],
    },
    // Debian, Ubuntu, Alpine (update-ca-certificates only picks up *.crt)
    SystemLayout {
        dir: "/usr/local/share/ca-certificates",
        extension: "crt",
        refresh: &["update-ca-certificates"],
    },
    // Arch, Manjaro
    SystemLayout {
        dir: "/etc/ca-certificates/trust-source/anchors",
        extension: "crt",
        refresh: &["trust", "extract-compat"],
    },
    // openSUSE
    SystemLayout {
        dir: "/usr/share/pki/trust/anchors",
        extension: "pem",
        refresh: &["update-ca-certificates"],
    },
];

impl SystemLayout {
    fn detect() -> Option<Self> {
        SYSTEM_LAYOUTS
            .iter()
            .copied()
            .find(|layout| Path::new(layout.dir).is_dir())
    }

    /// Where `install` writes the anchor.
    fn anchor_path(self) -> PathBuf {
        self.anchor_path_with_stem(ANCHOR_STEM)
    }

    /// Every anchor file this app may have written: current, then legacy.
    fn anchor_paths(self) -> [PathBuf; 2] {
        [
            self.anchor_path(),
            self.anchor_path_with_stem(LEGACY_ANCHOR_STEM),
        ]
    }

    fn anchor_path_with_stem(self, stem: &str) -> PathBuf {
        Path::new(self.dir).join(format!("{stem}.{}", self.extension))
    }
}

pub(super) fn is_installed(ca: &CertificateAuthority) -> Result<bool, TrustError> {
    let status = status(ca)?;
    Ok(status.app_trusted && status.system_trusted != Some(false))
}

/// Cider's trust (NSS) separately from the optional system anchor. An unknown
/// anchor layout yields `None`.
pub(super) fn status(ca: &CertificateAuthority) -> Result<TrustStatus, TrustError> {
    let mut nss = Vec::new();
    for db in nss_databases() {
        match nss_contains(&db) {
            Ok(trusted) => nss.push(trusted),
            Err(TrustError::ToolMissing { .. }) => nss.push(false),
            Err(err) => return Err(err),
        }
    }
    let anchor = SystemLayout::detect().map(|layout| anchor_matches(layout, ca));
    Ok(combine(&nss, anchor))
}

pub(super) fn install(ca: &CertificateAuthority) -> Result<(), TrustError> {
    // NSS first: it is what Cider actually uses, and needs no elevation.
    let mut dbs = nss_databases();
    if dbs.is_empty() {
        let default = default_nss_db()?;
        create_nss_db(&default)?;
        dbs.push(default);
    }
    for db in &dbs {
        if !nss_contains(db)? {
            nss_add(db, ca.cert_path())?;
        }
    }

    let Some(layout) = SystemLayout::detect() else {
        tracing::warn!("no known system trust-anchor directory; CA installed for NSS (Cider) only");
        return Ok(());
    };
    if !anchor_matches(layout, ca) {
        system_install(layout, ca.cert_path())?;
    }
    Ok(())
}

pub(super) fn uninstall(_ca: &CertificateAuthority) -> Result<(), TrustError> {
    for db in nss_databases() {
        // Several entries can share the nickname after repeated imports; the
        // bound guards against a tool that reports success without deleting.
        for _ in 0..MAX_NSS_DUPLICATES {
            let Some(nickname) = nss_nickname(&db)? else {
                break;
            };
            nss_delete(&db, nickname)?;
        }
    }
    if let Some(layout) = SystemLayout::detect()
        && layout.anchor_paths().iter().any(|path| path.exists())
    {
        system_uninstall(layout)?;
    }
    Ok(())
}

// ---- system anchors -------------------------------------------------------

fn anchor_matches(layout: SystemLayout, ca: &CertificateAuthority) -> bool {
    layout.anchor_paths().iter().any(|path| {
        fs::read_to_string(path).is_ok_and(|existing| existing.trim() == ca.cert_pem().trim())
    })
}

fn system_install(layout: SystemLayout, cert: &Path) -> Result<(), TrustError> {
    // Paths and the refresh command are passed as positional parameters, never
    // interpolated into the script.
    let mut args = vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        // Also drops a legacy-named anchor, so the CA is never anchored twice.
        OsString::from(r#"install -m 0644 -- "$1" "$2" && rm -f -- "$3" && shift 3 && exec "$@""#),
        OsString::from("sh"),
        cert.into(),
    ];
    args.extend(layout.anchor_paths().map(OsString::from));
    args.extend(layout.refresh.iter().map(OsString::from));
    run_elevated(&args)
}

fn system_uninstall(layout: SystemLayout) -> Result<(), TrustError> {
    let mut args = vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        // Removes the current and the legacy-named anchor in one prompt.
        OsString::from(r#"rm -f -- "$1" "$2" && shift 2 && exec "$@""#),
        OsString::from("sh"),
    ];
    args.extend(layout.anchor_paths().map(OsString::from));
    args.extend(layout.refresh.iter().map(OsString::from));
    run_elevated(&args)
}

fn run_elevated(args: &[OsString]) -> Result<(), TrustError> {
    let output = PKEXEC.run(args)?;
    match output.status.code() {
        Some(0) => Ok(()),
        Some(PKEXEC_DISMISSED) => Err(TrustError::Cancelled),
        _ => Err(PKEXEC.failure(&output)),
    }
}

// ---- NSS ------------------------------------------------------------------

fn home() -> Result<PathBuf, TrustError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| TrustError::Command {
            program: "certutil",
            status: "not run".to_owned(),
            stderr: "HOME is not set; cannot locate the NSS database".to_owned(),
        })
}

fn default_nss_db() -> Result<PathBuf, TrustError> {
    Ok(home()?.join(".pki/nssdb"))
}

/// Existing NSS databases that Cider may read.
fn nss_databases() -> Vec<PathBuf> {
    home().map_or_else(|_| Vec::new(), |home| nss_database_candidates(&home))
}

fn sql_db_arg(db: &Path) -> OsString {
    let mut arg = OsString::from("sql:");
    arg.push(db);
    arg
}

fn create_nss_db(db: &Path) -> Result<(), TrustError> {
    fs::create_dir_all(db).map_err(|e| TrustError::io(db, e))?;
    CERTUTIL.run_checked([
        OsString::from("-N"),
        OsString::from("-d"),
        sql_db_arg(db),
        OsString::from("--empty-password"),
    ])?;
    Ok(())
}

fn nss_contains(db: &Path) -> Result<bool, TrustError> {
    Ok(nss_nickname(db)?.is_some())
}

/// The first nickname this app has used for its CA that `db` holds an entry
/// under.
fn nss_nickname(db: &Path) -> Result<Option<&'static str>, TrustError> {
    for &nickname in KNOWN_CA_COMMON_NAMES {
        // `-L -n` exits non-zero when no certificate has that nickname.
        let output = CERTUTIL.run([
            OsString::from("-L"),
            OsString::from("-d"),
            sql_db_arg(db),
            OsString::from("-n"),
            OsString::from(nickname),
        ])?;
        if output.status.success() {
            return Ok(Some(nickname));
        }
    }
    Ok(None)
}

fn nss_add(db: &Path, cert: &Path) -> Result<(), TrustError> {
    CERTUTIL.run_checked([
        OsString::from("-A"),
        OsString::from("-d"),
        sql_db_arg(db),
        // Trusted CA for issuing TLS server certificates.
        OsString::from("-t"),
        OsString::from("C,,"),
        OsString::from("-n"),
        OsString::from(CA_COMMON_NAME),
        OsString::from("-i"),
        cert.into(),
    ])?;
    Ok(())
}

fn nss_delete(db: &Path, nickname: &str) -> Result<(), TrustError> {
    CERTUTIL.run_checked([
        OsString::from("-D"),
        OsString::from("-d"),
        sql_db_arg(db),
        OsString::from("-n"),
        OsString::from(nickname),
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debian_anchor_should_use_crt_extension() {
        let debian = SYSTEM_LAYOUTS
            .iter()
            .find(|l| l.dir == "/usr/local/share/ca-certificates")
            .unwrap();
        assert_eq!(
            debian.anchor_path(),
            Path::new("/usr/local/share/ca-certificates/cider-offline-ca.crt")
        );
    }

    #[test]
    fn anchor_paths_should_include_legacy_stem_after_current() {
        let fedora = SYSTEM_LAYOUTS[0];
        assert_eq!(
            fedora.anchor_paths(),
            [
                PathBuf::from("/etc/pki/ca-trust/source/anchors/cider-offline-ca.pem"),
                PathBuf::from("/etc/pki/ca-trust/source/anchors/cider-offline-proxy-ca.pem"),
            ]
        );
    }

    #[test]
    fn sql_db_arg_should_prefix_sql_scheme() {
        assert_eq!(
            sql_db_arg(Path::new("/home/u/.pki/nssdb")),
            "sql:/home/u/.pki/nssdb"
        );
    }
}
