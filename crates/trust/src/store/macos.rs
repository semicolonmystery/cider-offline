//! macOS: the user's login keychain plus per-user trust settings, managed with
//! the `security` command-line tool.
//!
//! Changing per-user trust settings makes macOS show an authentication dialog,
//! which is the user's consent. No `sudo` is needed. Chromium-based apps
//! (Cider) honour user-domain trust settings.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;

use super::command::Tool;
use crate::ca::KNOWN_CA_COMMON_NAMES;
use crate::{CertificateAuthority, TrustError};

const SECURITY: Tool = Tool {
    program: "security",
    missing_hint: "it ships with macOS at /usr/bin/security",
};

pub(super) fn is_installed(ca: &CertificateAuthority) -> Result<bool, TrustError> {
    // `verify-cert` succeeds for a self-signed root only when it is trusted.
    // The policy must match the `-p ssl` constraint used by `install`.
    let mut args: Vec<OsString> = ["verify-cert", "-q", "-L", "-l", "-p", "ssl", "-c"]
        .map(OsString::from)
        .into();
    args.push(ca.cert_path().into());
    Ok(SECURITY.run(args)?.status.success())
}

pub(super) fn install(ca: &CertificateAuthority) -> Result<(), TrustError> {
    let keychain = login_keychain()?;
    let mut args: Vec<OsString> = ["add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k"]
        .map(OsString::from)
        .into();
    args.push(keychain.into());
    args.push(ca.cert_path().into());
    let output = SECURITY.run(args)?;
    check(&output)
}

pub(super) fn uninstall(ca: &CertificateAuthority) -> Result<(), TrustError> {
    let keychain = login_keychain()?;
    let fingerprint = ca.sha1_fingerprint_hex();

    if in_keychain(&keychain, &fingerprint)? {
        // `-t` also removes the certificate's user trust settings.
        let mut args: Vec<OsString> = ["delete-certificate", "-t", "-Z"]
            .map(OsString::from)
            .into();
        args.push(fingerprint.into());
        args.push(keychain.into());
        check(&SECURITY.run(args)?)?;
    }

    // Trust settings can outlive the keychain item (e.g. the user deleted the
    // certificate by hand); clear them too.
    if is_installed(ca)? {
        let mut args = vec![OsString::from("remove-trusted-cert")];
        args.push(ca.cert_path().into());
        check(&SECURITY.run(args)?)?;
    }
    Ok(())
}

/// Whether the login keychain holds a certificate with this SHA-1 hash under
/// any name this app has used for its CA.
fn in_keychain(keychain: &Path, sha1_hex: &str) -> Result<bool, TrustError> {
    for &name in KNOWN_CA_COMMON_NAMES {
        let mut args: Vec<OsString> = ["find-certificate", "-a", "-Z", "-c", name]
            .map(OsString::from)
            .into();
        args.push(keychain.into());
        let output = SECURITY.run(args)?;
        // Exit status is non-zero when nothing matches the name.
        if output.status.success()
            && listing_contains_sha1(&String::from_utf8_lossy(&output.stdout), sha1_hex)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Parses `security find-certificate -Z` output for a `SHA-1 hash: <hex>` line.
fn listing_contains_sha1(listing: &str, sha1_hex: &str) -> bool {
    listing.lines().any(|line| {
        line.trim()
            .strip_prefix("SHA-1 hash:")
            .is_some_and(|hash| hash.trim().eq_ignore_ascii_case(sha1_hex))
    })
}

fn login_keychain() -> Result<PathBuf, TrustError> {
    let home = std::env::var_os("HOME").ok_or_else(|| TrustError::Command {
        program: "security",
        status: "not run".to_owned(),
        stderr: "HOME is not set; cannot locate the login keychain".to_owned(),
    })?;
    Ok(PathBuf::from(home).join("Library/Keychains/login.keychain-db"))
}

fn check(output: &Output) -> Result<(), TrustError> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_cancellation(&stderr) {
        Err(TrustError::Cancelled)
    } else {
        Err(SECURITY.failure(output))
    }
}

/// `security` reports a dismissed authorization dialog as e.g.
/// "The authorization was canceled by the user."
fn is_cancellation(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("canceled") || lower.contains("cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "keychain: \"/Users/me/Library/Keychains/login.keychain-db\"\n\
        SHA-256 hash: 00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF\n\
        SHA-1 hash: 0123456789ABCDEF0123456789ABCDEF01234567\n\
        version: 512\n";

    #[test]
    fn listing_contains_sha1_should_match_case_insensitively() {
        assert!(listing_contains_sha1(
            LISTING,
            "0123456789abcdef0123456789abcdef01234567"
        ));
    }

    #[test]
    fn listing_contains_sha1_should_not_match_other_hash() {
        assert!(!listing_contains_sha1(
            LISTING,
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
        ));
    }

    #[test]
    fn is_cancellation_should_detect_authorization_cancel() {
        assert!(is_cancellation(
            "SecTrustSettingsSetTrustSettings: The authorization was canceled by the user."
        ));
    }

    #[test]
    fn is_installed_should_be_false_for_fresh_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = CertificateAuthority::load_or_create(dir.path()).unwrap();
        assert!(!is_installed(&ca).unwrap());
    }
}
