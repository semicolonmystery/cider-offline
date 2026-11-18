//! Persistent settings: `config.toml` and launch-at-login registration.

use std::path::{Path, PathBuf};

use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode, WindowsEnableMode};
use cider_offline_core::Config;

/// Command-line flag passed by the autostart entry.
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Autostart entry name on Windows and Linux.
const AUTOSTART_NAME: &str = "Cider Offline";

/// macOS `LaunchAgent` label: reverse-DNS, no spaces.
const MACOS_AUTOSTART_LABEL: &str = "sh.cider-offline.CiderOffline";

/// Writes `config` to `path` on the blocking pool, logging failures.
pub async fn save_config(config: Config, path: PathBuf) -> Result<(), String> {
    let result = tokio::task::spawn_blocking(move || config.save(&path)).await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::error!(%error, "could not save config");
            Err(error.to_string())
        }
        Err(error) => Err(format!("config task failed: {error}")),
    }
}

/// Launch-at-login registration for this executable.
pub struct Autostart {
    inner: AutoLaunch,
}

impl Autostart {
    /// Builds the registration for the running executable, or `None` when
    /// the platform or executable location is unsupported.
    pub fn for_current_exe() -> Option<Self> {
        if !AutoLaunch::is_support() {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        let app_path = launch_target(&exe, appimage.as_deref());
        let app_path = app_path.to_str()?;
        let inner = AutoLaunchBuilder::new()
            .set_app_name(autostart_name())
            .set_app_path(app_path)
            .set_args(&[MINIMIZED_FLAG])
            .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
            .set_bundle_identifiers(&[autostart_name()])
            .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
            .build()
            .inspect_err(|error| tracing::warn!(%error, "autostart unavailable"))
            .ok()?;
        Some(Self { inner })
    }

    /// Whether launch-at-login is currently registered.
    ///
    /// # Errors
    ///
    /// Returns the reason when the OS entry could not be read.
    pub fn is_enabled(&self) -> Result<bool, String> {
        self.inner.is_enabled().map_err(|error| error.to_string())
    }

    /// Registers or unregisters launch-at-login.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let result = if enabled {
            self.inner.enable()
        } else {
            self.inner.disable()
        };
        result.map_err(|error| error.to_string())
    }
}

/// The command the login entry runs: the `AppImage` on Linux when launched from
/// one (the mounted binary disappears on exit), otherwise the executable. On
/// macOS that is the binary inside the `.app` bundle, which the `LaunchAgent`
/// plist runs directly.
fn launch_target(exe: &Path, appimage: Option<&Path>) -> PathBuf {
    if cfg!(target_os = "linux")
        && let Some(appimage) = appimage
    {
        return appimage.to_path_buf();
    }
    exe.to_path_buf()
}

/// Entry name: the `LaunchAgent` label on macOS, the app name elsewhere.
fn autostart_name() -> &'static str {
    if cfg!(target_os = "macos") {
        MACOS_AUTOSTART_LABEL
    } else {
        AUTOSTART_NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn launch_target_should_use_inner_binary_on_macos() {
        let exe = Path::new("/Applications/Cider Offline.app/Contents/MacOS/cider-offline");
        assert_eq!(launch_target(exe, None), exe);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn autostart_name_should_be_reverse_dns() {
        assert!(!autostart_name().contains(' '));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn launch_target_should_prefer_appimage() {
        assert_eq!(
            launch_target(
                Path::new("/tmp/.mount/usr/bin/x"),
                Some(Path::new("/home/u/My Apps/C.AppImage"))
            ),
            Path::new("/home/u/My Apps/C.AppImage")
        );
    }

    #[test]
    #[cfg(windows)]
    fn launch_target_should_use_executable_on_windows() {
        let exe = Path::new(r"C:\Program Files\Cider Offline\cider-offline.exe");
        assert_eq!(launch_target(exe, None), exe);
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "writes HKCU Run; run manually"]
    fn run_key_should_quote_path_with_spaces() {
        let name = format!("CiderOfflineTest-{}", std::process::id());
        let auto = AutoLaunchBuilder::new()
            .set_app_name(&name)
            .set_app_path(r"C:\Temp With Space\x y.exe")
            .set_args(&[MINIMIZED_FLAG])
            .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
            .build()
            .expect("build");
        let query = || {
            std::process::Command::new("reg")
                .args([
                    "query",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    &name,
                ])
                .output()
                .expect("reg")
        };
        auto.enable().expect("enable");
        let stdout = String::from_utf8_lossy(&query().stdout).into_owned();
        let disabled = auto.disable();
        let after = query();
        assert!(
            stdout.contains(r#""C:\Temp With Space\x y.exe" --minimized"#),
            "{stdout}"
        );
        disabled.expect("disable");
        assert!(!after.status.success(), "entry still present");
    }
}
