//! Pure NSS database discovery and trust aggregation (Linux logic, tested on
//! every OS).

use std::fs;
use std::path::{Path, PathBuf};

use super::TrustStatus;

/// Existing NSS databases that Cider may read: the default user database, the
/// Flatpak sandbox (`~/.var/app/<id>/.pki/nssdb`, id like `sh.cider.Cider`) and
/// Snap (`~/snap/<name>/current/.pki/nssdb`) ones whose name contains "cider".
pub(super) fn nss_database_candidates(home: &Path) -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    let default = home.join(".pki/nssdb");
    if is_nss_db(&default) {
        dbs.push(default);
    }
    dbs.extend(app_dbs(&home.join(".var/app"), ""));
    dbs.extend(app_dbs(&home.join("snap"), "current"));
    dbs
}

/// `<root>/<cider-named dir>/<sub>/.pki/nssdb` entries that exist, sorted.
fn app_dbs(root: &Path, sub: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dbs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| is_cider_app_id(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path().join(sub).join(".pki/nssdb"))
        .filter(|db| is_nss_db(db))
        .collect();
    dbs.sort();
    dbs
}

fn is_cider_app_id(app_id: &str) -> bool {
    app_id.to_ascii_lowercase().contains("cider")
}

/// Modern ("sql:") NSS databases are identified by `cert9.db`.
fn is_nss_db(dir: &Path) -> bool {
    dir.join("cert9.db").is_file()
}

/// `app_trusted` needs at least one database and the CA in every one.
pub(super) fn combine(nss: &[bool], anchor: Option<bool>) -> TrustStatus {
    TrustStatus {
        app_trusted: !nss.is_empty() && nss.iter().all(|&trusted| trusted),
        system_trusted: anchor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_db(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("cert9.db"), b"").unwrap();
    }

    #[test]
    fn nss_candidates_should_include_snap_and_flatpak_cider() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        touch_db(&h.join(".pki/nssdb"));
        touch_db(&h.join(".var/app/sh.cider.Cider/.pki/nssdb"));
        touch_db(&h.join("snap/cider/current/.pki/nssdb"));
        touch_db(&h.join("snap/firefox/current/.pki/nssdb"));
        let mut found = nss_database_candidates(h);
        found.sort();
        let mut expected = vec![
            h.join(".pki/nssdb"),
            h.join(".var/app/sh.cider.Cider/.pki/nssdb"),
            h.join("snap/cider/current/.pki/nssdb"),
        ];
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn nss_candidates_should_be_empty_without_databases() {
        let home = tempfile::tempdir().unwrap();
        assert!(nss_database_candidates(home.path()).is_empty());
    }

    #[test]
    fn combine_should_trust_app_without_system_anchor() {
        assert_eq!(
            combine(&[true, true], Some(false)),
            TrustStatus {
                app_trusted: true,
                system_trusted: Some(false)
            }
        );
    }

    #[test]
    fn combine_should_not_trust_without_databases() {
        assert!(!combine(&[], None).app_trusted);
    }

    #[test]
    fn combine_should_not_trust_when_any_database_lacks_ca() {
        assert!(!combine(&[true, false], Some(true)).app_trusted);
    }

    #[test]
    fn is_cider_app_id_should_match_flatpak_id_case_insensitively() {
        assert!(is_cider_app_id("sh.Cider.Genten"));
    }

    #[test]
    fn is_cider_app_id_should_ignore_other_apps() {
        assert!(!is_cider_app_id("org.mozilla.firefox"));
    }

    #[test]
    fn is_nss_db_should_require_cert9_db() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_nss_db(dir.path()));
    }
}
