//! Integration test that exercises the *real* operating-system trust store.
//!
//! It installs the CA, confirms it is trusted, then removes it again. Because
//! this mutates the machine's trust store and triggers an OS consent prompt
//! (the Windows security warning, the macOS authorization dialog, or a polkit
//! prompt on Linux), it is marked `#[ignore]` so a plain `cargo test` never
//! touches the store. Run it deliberately, on a machine you are willing to
//! prompt, with:
//!
//! ```text
//! cargo test -p cider-offline-trust --test system_store -- --ignored --nocapture
//! ```

use cider_offline_trust::{CertificateAuthority, TrustStore as _, system_trust_store};

/// Full install -> verify -> uninstall round trip against the live OS store.
#[test]
#[ignore = "mutates the real OS trust store and shows an OS consent prompt; run manually with --ignored"]
fn install_then_uninstall_round_trip() {
    let dir = tempfile::tempdir().expect("create temp CA dir");
    let ca = CertificateAuthority::load_or_create(dir.path()).expect("generate CA");
    let store = system_trust_store();

    // A freshly generated CA must not already be trusted.
    assert!(
        !store.is_installed(&ca).expect("initial lookup"),
        "a brand-new CA should not be trusted yet"
    );

    store.install(&ca).expect("install CA into OS trust store");
    assert!(
        store.is_installed(&ca).expect("post-install lookup"),
        "CA should be trusted after install"
    );

    // Installing again must be a no-op, not an error (idempotency).
    store
        .install(&ca)
        .expect("second install should be idempotent");

    store
        .uninstall(&ca)
        .expect("uninstall CA from OS trust store");
    assert!(
        !store.is_installed(&ca).expect("post-uninstall lookup"),
        "CA should no longer be trusted after uninstall"
    );

    // Uninstalling again must also be a no-op.
    store
        .uninstall(&ca)
        .expect("second uninstall should be idempotent");
}
