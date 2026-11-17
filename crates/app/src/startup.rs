//! Startup: locating the folders and the `--version` flag.

use std::io::{self, Write as _};
use std::path::Path;

use anyhow::Context as _;
use cider_offline_core::AppPaths;

/// Prints `cider-offline <version>` and exits.
pub const VERSION_FLAG: &str = "--version";

/// Folders the app uses in this run (portable or installed).
pub fn discover_paths() -> anyhow::Result<AppPaths> {
    AppPaths::discover().context("locating app directories")
}

/// Folder name in the path macOS runs a quarantined app from when it
/// "translocates" it to a random read-only location.
const TRANSLOCATION_DIR: &str = "AppTranslocation";

/// Whether `exe` runs from a macOS App Translocation folder, where a
/// portable `data` folder next to the `.app` cannot be found.
fn is_translocated(exe: &Path) -> bool {
    exe.components()
        .any(|component| component.as_os_str() == TRANSLOCATION_DIR)
}

/// Whether this run is App-Translocated (macOS only; always `false` elsewhere).
pub fn running_translocated() -> bool {
    cfg!(target_os = "macos") && std::env::current_exe().is_ok_and(|exe| is_translocated(&exe))
}

/// Logs a warning when [`running_translocated`]: the portable `data` folder
/// is then silently not used.
pub fn warn_if_translocated() {
    if running_translocated() {
        tracing::warn!(
            "macOS App Translocation is running this copy from a read-only temporary folder, \
             so a portable `data` folder next to the app is not used; run \
             `xattr -dr com.apple.quarantine` on the unzipped folder and start it again"
        );
    }
}

/// Whether `args` ask for [`VERSION_FLAG`].
pub fn version_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == VERSION_FLAG)
}

/// The version line printed for [`VERSION_FLAG`].
fn version_line() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

/// Prints the version line for [`VERSION_FLAG`].
pub fn print_version() -> anyhow::Result<()> {
    writeln!(io::stdout(), "{}", version_line())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn version_requested_should_match_only_the_exact_flag() {
        assert!(version_requested(&args(&["app", "--version"])));
        assert!(version_requested(&args(&[
            "app",
            "--minimized",
            "--version"
        ])));
        assert!(!version_requested(&args(&["app"])));
        assert!(!version_requested(&args(&["app", "--versions", "-V"])));
    }

    #[test]
    fn is_translocated_should_match_the_translocation_folder() {
        let exe = Path::new(
            "/private/var/folders/x/T/AppTranslocation/1A2B/d/Cider Offline.app/Contents/MacOS/cider-offline",
        );

        assert!(is_translocated(exe));
    }

    #[test]
    fn is_translocated_should_ignore_normal_paths() {
        let exe = Path::new("/Applications/Cider Offline.app/Contents/MacOS/cider-offline");

        assert!(!is_translocated(exe));
    }

    #[test]
    fn version_line_should_name_package_and_version() {
        assert_eq!(
            version_line(),
            concat!("cider-offline ", env!("CARGO_PKG_VERSION"))
        );
    }
}
