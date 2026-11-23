//! Integration with the installed Cider client.
//!
//! The Windows build of Cider (.NET / WPF / `WebView2`) keeps its own
//! proxy setting in `%APPDATA%\sh.cider.dotnet\client-options.yml`. The keys
//! `UseProxy` and `ProxyServer` are translated into a `WebView2`
//! `--proxy-server=` switch when Cider starts, so toggling them routes Cider
//! through (or around) this proxy. The value is read only at startup and Cider
//! re-serializes the file on exit, so edits must be made **while Cider is
//! closed** and take effect on the next launch.
//!
//! Only Windows is [`Support::Automatic`]. The macOS and Linux builds are
//! Electron apps with no documented proxy key in their config, so there the
//! app only shows
//! [`manual_guide`] and never writes a Cider file or touches its process. The
//! stop/launch code for those OSes is deliberately not written: add it only
//! when [`SUPPORT`] flips to `Automatic` for that OS.
//!
//! The file is edited in place with a targeted, line-scoped editor rather than
//! a full YAML (de)serialize: Cider owns the file and writes ~30 unrelated
//! keys with `YamlDotNet`, and a round-trip through a serializer would
//! normalize formatting and drop its exact scalar styles. Editing only the two
//! proxy lines keeps every other byte intact (see the round-trip tests).

#[cfg(any(windows, test))]
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Name of Cider's main process (Windows).
#[cfg(windows)]
const CIDER_PROCESS: &str = "Cider.exe";

/// How far this app can drive Cider on the current OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// The app edits Cider's config and restarts it.
    Automatic,
    /// The app only shows [`manual_guide`].
    GuideOnly,
}

/// Support per OS: only the Windows build has a known proxy key.
pub const SUPPORT: Support = if cfg!(windows) {
    Support::Automatic
} else {
    Support::GuideOnly
};

/// Steps for pointing Cider at this proxy by hand, with `listen` (this
/// proxy's configured listen address) filled in. Keep it in step with the
/// "Connecting Cider" chapter of the user guide.
#[cfg(windows)]
pub fn manual_guide(listen: &str) -> String {
    format!(
        "Turn on \"Route Cider through this proxy\", or set UseProxy: true and \
ProxyServer: {listen} in Cider's client-options.yml while Cider is closed. \
Trust this proxy's certificate (Certificate row above)."
    )
}

/// Steps for pointing Cider at this proxy by hand, with `listen` (this
/// proxy's configured listen address) filled in. Keep it in step with the
/// "Connecting Cider" chapter of the user guide.
#[cfg(target_os = "macos")]
pub fn manual_guide(listen: &str) -> String {
    format!(
        "Quit Cider, then start it from Terminal with: open -a Cider --args --proxy-server={listen}. \
Or set the proxy in System Settings > Network > Proxies (this affects every app). \
Trust this proxy's certificate (Certificate row above)."
    )
}

/// Steps for pointing Cider at this proxy by hand, with `listen` (this
/// proxy's configured listen address) filled in. Keep it in step with the
/// "Connecting Cider" chapter of the user guide.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn manual_guide(listen: &str) -> String {
    format!(
        "Quit Cider, then start it with: cider --proxy-server={listen}. \
For the Flatpak: flatpak run sh.cider.Cider --proxy-server={listen}. \
Or set the proxy in your desktop's network settings. \
Trust this proxy's certificate (Certificate row above)."
    )
}

/// Whether a process is Cider itself: the process name or the executable's
/// file stem is `cider`, case-insensitively. Never matches this app
/// (`cider-offline`).
#[cfg(any(windows, test))]
fn is_cider_process(name: &OsStr, exe: Option<&Path>) -> bool {
    let is_cider = |path: &Path| {
        path.file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("cider"))
    };
    is_cider(Path::new(name)) || exe.is_some_and(is_cider)
}

/// Default config locations for this OS, in priority order.
fn default_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| {
                vec![
                    PathBuf::from(appdata)
                        .join("sh.cider.dotnet")
                        .join("client-options.yml"),
                ]
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Finds Cider's config: the user's chosen file if it exists, else the OS
/// default locations.
pub fn locate(chosen: Option<&Path>) -> Result<PathBuf, CiderError> {
    locate_in(chosen, &default_candidates())
}

fn locate_in(chosen: Option<&Path>, defaults: &[PathBuf]) -> Result<PathBuf, CiderError> {
    chosen
        .filter(|path| path.is_file())
        .map(Path::to_path_buf)
        .or_else(|| defaults.iter().find(|path| path.is_file()).cloned())
        .ok_or_else(|| {
            CiderError::ConfigNotFound(
                chosen
                    .map(Path::to_path_buf)
                    .or_else(|| defaults.first().cloned()),
            )
        })
}

/// Cider's spatial audio (Dolby Atmos) setting: `audio.atmos.enabled` in
/// `spa-config.yml`, which sits next to `client_options` (Cider's
/// `client-options.yml`). `None` when the file or the key is missing.
///
/// Read only, never written: Cider owns the file.
pub fn spatial_audio_enabled(client_options: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(client_options.with_file_name(SPA_CONFIG)).ok()?;
    atmos_enabled(&text)
}

/// File name of Cider's web-app settings (`audio`, `visual`, ...).
const SPA_CONFIG: &str = "spa-config.yml";

/// Finds `audio.atmos.enabled` in a block-style YAML document by
/// indentation, without a YAML parser (as with `client-options.yml`, only
/// the one key matters).
fn atmos_enabled(text: &str) -> Option<bool> {
    const PATH: [&str; 3] = ["audio", "atmos", "enabled"];
    let mut stack: Vec<(usize, &str)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        while stack.last().is_some_and(|(level, _)| *level >= indent) {
            stack.pop();
        }
        let key = key.trim().trim_matches(['"', '\'']);
        let value = value.trim();
        let parents: Vec<&str> = stack.iter().map(|(_, k)| *k).collect();
        if parents == PATH[..2] && key == PATH[2] {
            return match value.split('#').next().unwrap_or_default().trim() {
                "true" | "True" | "TRUE" => Some(true),
                "false" | "False" | "FALSE" => Some(false),
                _ => None,
            };
        }
        if value.is_empty() {
            stack.push((indent, key));
        }
    }
    None
}

/// Read-only snapshot of Cider's proxy configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiderProxyState {
    /// Value of `UseProxy` (`false` when absent or unparseable).
    pub use_proxy: bool,
    /// Value of `ProxyServer` (empty when absent).
    pub proxy_server: String,
    /// Whether `ProxyServer` equals this proxy's listening address.
    pub points_at_us: bool,
}

/// Result of [`apply_toggle`].
// Constructed only by the Windows `apply_toggle`; the stub elsewhere always
// returns `Unsupported`.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Whether Cider was running (and therefore stopped and relaunched).
    pub was_running: bool,
    /// Whether the proxy was turned on (`true`) or off (`false`).
    pub enabled: bool,
}

/// Errors from Cider integration.
#[derive(Debug)]
pub enum CiderError {
    /// No config file exists; carries the path that was looked for, if any.
    ConfigNotFound(Option<PathBuf>),
    /// The Cider executable could not be located.
    // Constructed only by the Windows stop/launch code; dead elsewhere.
    #[cfg_attr(not(windows), allow(dead_code))]
    ExecutableNotFound,
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// Stopping Cider did not complete in time.
    #[cfg_attr(not(windows), allow(dead_code))]
    StopTimedOut,
    /// Launching Cider failed.
    #[cfg_attr(not(windows), allow(dead_code))]
    LaunchFailed(String),
    /// The feature is not available on this platform.
    // Constructed only by the non-Windows stubs; dead on Windows.
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
}

impl std::fmt::Display for CiderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigNotFound(Some(path)) => {
                write!(f, "Cider's config was not found at {}", path.display())
            }
            Self::ConfigNotFound(None) => f.write_str("Cider's config file was not found"),
            Self::ExecutableNotFound => f.write_str("could not find the Cider executable"),
            Self::Io(error) => write!(f, "file error: {error}"),
            Self::StopTimedOut => f.write_str("Cider did not close in time"),
            Self::LaunchFailed(reason) => write!(f, "could not launch Cider: {reason}"),
            Self::Unsupported => f.write_str("the app cannot change Cider's settings on this OS"),
        }
    }
}

impl std::error::Error for CiderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Platform-independent YAML read/edit helpers (also used by the tests on every
// platform). These operate on strings so they are trivially testable and never
// reserialize the whole document.
// ---------------------------------------------------------------------------

/// Splits a line into its body and its line terminator (`\r\n`, `\n`, `\r`
/// or empty for the final line).
fn split_terminator(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else if let Some(body) = line.strip_suffix('\r') {
        (body, "\r")
    } else {
        (line, "")
    }
}

/// Whether `body` is a top-level (unindented) mapping entry for `key`.
fn is_top_level_key(body: &str, key: &str) -> bool {
    if body.starts_with([' ', '\t']) {
        return false;
    }
    match body.strip_prefix(key) {
        Some(rest) => rest.trim_start_matches(' ').starts_with(':'),
        None => false,
    }
}

/// Strips one matching pair of surrounding quotes, if present.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if value.len() >= 2 {
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

/// Reads the value of a top-level scalar `key`, if present.
fn read_scalar(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let (body, _) = split_terminator(line);
        if is_top_level_key(body, key) {
            let after_colon = &body[body.find(':')? + 1..];
            Some(unquote(after_colon.trim()))
        } else {
            None
        }
    })
}

/// Parses a YAML boolean scalar (as written by `YamlDotNet`).
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Returns `content` with top-level `key` set to `value`, preserving every
/// other line verbatim. The key is appended (using the file's dominant line
/// ending) when it is absent.
#[cfg(any(windows, test))]
fn set_scalar(content: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(content.len() + key.len() + value.len() + 4);
    let mut found = false;
    for line in content.split_inclusive('\n') {
        let (body, term) = split_terminator(line);
        if !found && is_top_level_key(body, key) {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push_str(term);
            found = true;
        } else {
            out.push_str(line);
        }
    }
    if !found {
        let newline = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value);
        out.push_str(newline);
    }
    out
}

/// Produces the edited config text for a toggle.
///
/// Enabling sets both `UseProxy: true` and `ProxyServer`. Disabling only flips
/// `UseProxy: false`, leaving `ProxyServer` intact, so turning the proxy back
/// on later (by hand or from Cider) still points at the right address.
#[cfg(any(windows, test))]
fn edit_config(content: &str, enabled: bool, server: &str) -> String {
    let with_flag = set_scalar(content, "UseProxy", if enabled { "true" } else { "false" });
    if enabled {
        set_scalar(&with_flag, "ProxyServer", server)
    } else {
        with_flag
    }
}

/// Derives the read-only proxy state from config text.
fn state_from_text(content: &str, our_server: &str) -> CiderProxyState {
    let use_proxy = read_scalar(content, "UseProxy")
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let proxy_server = read_scalar(content, "ProxyServer").unwrap_or_default();
    let points_at_us = !our_server.is_empty() && proxy_server == our_server;
    CiderProxyState {
        use_proxy,
        proxy_server,
        points_at_us,
    }
}

// ---------------------------------------------------------------------------
// Path- and fs-level helpers (testable with an explicit path on any platform).
// ---------------------------------------------------------------------------

/// Writes a toggle to a specific `client-options.yml` path, atomically and
/// preserving all other keys.
#[cfg(any(windows, test))]
fn set_proxy_at(path: &std::path::Path, enabled: bool, server: &str) -> Result<(), CiderError> {
    let content = read_config(path)?;
    let updated = edit_config(&content, enabled, server);
    write_atomic(path, &updated)
}

/// Reads a config file, mapping "not found" to [`CiderError::ConfigNotFound`].
fn read_config(path: &std::path::Path) -> Result<String, CiderError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(CiderError::ConfigNotFound(Some(path.to_path_buf())))
        }
        Err(error) => Err(CiderError::Io(error)),
    }
}

/// Writes `content` to `path` atomically via a sibling temp file + rename.
#[cfg(any(windows, test))]
fn write_atomic(path: &std::path::Path, content: &str) -> Result<(), CiderError> {
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp_path = PathBuf::from(temp_name);
    std::fs::write(&temp_path, content).map_err(CiderError::Io)?;
    // On Windows `rename` maps to `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`,
    // so this atomically replaces the existing file on the same volume.
    match std::fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Best-effort cleanup so a failed write leaves no stray temp file.
            let _ = std::fs::remove_file(&temp_path);
            Err(CiderError::Io(error))
        }
    }
}

// ---------------------------------------------------------------------------
// Windows implementation.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use sysinfo::{ProcessesToUpdate, System};

    use super::{ApplyOutcome, CIDER_PROCESS, CiderError};

    /// How long to wait for a graceful close before forcing termination.
    const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long to wait for a forced termination to complete.
    const FORCE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Poll interval while waiting for Cider to exit.
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    /// `CREATE_NO_WINDOW`: keep `taskkill` from flashing a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// Candidate install locations, in priority order.
    fn install_candidates() -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(dir) = std::env::var_os(var) {
                candidates.push(PathBuf::from(dir).join("Cider").join(CIDER_PROCESS));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("Cider")
                    .join(CIDER_PROCESS),
            );
        }
        candidates
    }

    /// Refreshes the process table into a fresh [`System`]. Returns `None` if
    /// the platform could not provide system information.
    fn process_snapshot() -> Option<System> {
        let mut system = System::new().ok()?;
        system.refresh_processes(ProcessesToUpdate::All, true);
        Some(system)
    }

    /// Path of the running Cider executable, if any.
    fn running_cider_exe() -> Option<PathBuf> {
        process_snapshot()?
            .processes()
            .values()
            .find_map(|process| {
                if super::is_cider_process(process.name(), process.exe()) {
                    process.exe().map(Path::to_path_buf)
                } else {
                    None
                }
            })
    }

    /// Resolves the Cider executable: prefer the running instance, then known
    /// install locations.
    pub fn executable_path() -> Result<PathBuf, CiderError> {
        if let Some(path) = running_cider_exe() {
            return Ok(path);
        }
        install_candidates()
            .into_iter()
            .find(|candidate| candidate.is_file())
            .ok_or(CiderError::ExecutableNotFound)
    }

    /// Whether any `Cider.exe` process is currently running.
    pub fn is_cider_running() -> bool {
        process_snapshot().is_some_and(|system| {
            system
                .processes()
                .values()
                .any(|process| super::is_cider_process(process.name(), process.exe()))
        })
    }

    /// Runs `taskkill` for `Cider.exe`, optionally forcing. Spawn/IO failures
    /// propagate; a non-zero exit (e.g. "process not found") is not an error
    /// here because success is confirmed by polling the process table.
    fn taskkill(force: bool) -> Result<(), CiderError> {
        use std::os::windows::process::CommandExt as _;

        let mut command = std::process::Command::new("taskkill");
        command.args(["/IM", CIDER_PROCESS, "/T"]);
        if force {
            command.arg("/F");
        }
        command.creation_flags(CREATE_NO_WINDOW);
        command.output().map_err(CiderError::Io)?;
        Ok(())
    }

    /// Polls until Cider is gone or `timeout` elapses.
    fn wait_for_exit(timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if !is_cider_running() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Stops Cider: request a graceful close first, forcing only as a last
    /// resort after the graceful timeout.
    pub fn stop_cider() -> Result<(), CiderError> {
        if !is_cider_running() {
            return Ok(());
        }
        taskkill(false)?;
        if wait_for_exit(GRACEFUL_TIMEOUT) {
            return Ok(());
        }
        taskkill(true)?;
        if wait_for_exit(FORCE_TIMEOUT) {
            Ok(())
        } else {
            Err(CiderError::StopTimedOut)
        }
    }

    /// Launches Cider (detached) from its resolved install location.
    pub fn launch_cider() -> Result<(), CiderError> {
        let exe = executable_path()?;
        let mut command = std::process::Command::new(&exe);
        if let Some(parent) = exe.parent() {
            command.current_dir(parent);
        }
        command
            .spawn()
            .map_err(|error| CiderError::LaunchFailed(error.to_string()))?;
        Ok(())
    }

    /// Orchestrates a toggle: detect → (if running) stop → write → relaunch.
    ///
    /// The config write always happens while Cider is closed. When Cider was
    /// not running the config is simply written and Cider is left closed so
    /// the change applies the next time the user starts it.
    pub fn apply_toggle(
        config: &Path,
        enabled: bool,
        server: &str,
    ) -> Result<ApplyOutcome, CiderError> {
        let was_running = is_cider_running();
        if was_running {
            stop_cider()?;
        }
        super::set_proxy_at(config, enabled, server)?;
        if was_running {
            launch_cider()?;
        }
        Ok(ApplyOutcome {
            was_running,
            enabled,
        })
    }
}

// The orchestrating entry points used by the UI. The lower-level process and
// file operations (`config_path`, `executable_path`, `set_proxy`,
// `stop_cider`, `launch_cider`) stay encapsulated in `platform` and are driven
// through `apply_toggle`.
#[cfg(windows)]
pub use platform::{apply_toggle, is_cider_running};

/// Reads Cider's proxy state from the config file at `config`.
pub fn read_state(config: &Path, our_server: &str) -> Result<CiderProxyState, CiderError> {
    let content = read_config(config)?;
    Ok(state_from_text(&content, our_server))
}

// ---------------------------------------------------------------------------
// GuideOnly stubs: the app never touches Cider off Windows (see module docs).
// ---------------------------------------------------------------------------

/// Whether Cider is running (always `false` off Windows).
#[cfg(not(windows))]
pub fn is_cider_running() -> bool {
    false
}

/// Applies a Cider proxy toggle (Windows only).
#[cfg(not(windows))]
pub fn apply_toggle(
    _config: &Path,
    _enabled: bool,
    _server: &str,
) -> Result<ApplyOutcome, CiderError> {
    Err(CiderError::Unsupported)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::*;

    /// The shape of Cider's `spa-config.yml` around the setting (2026-11-20).
    const SPA_FIXTURE: &str = "visual:\r\n  sounds:\r\n    enabled: false\r\n\
audio:\r\n  volume: 1\r\n  atmos:\r\n    enabled: true\r\n    binaural: true\r\n\
    devicePreferences:\r\n      default: binaural\r\n  crossfade:\r\n    enabled: false\r\n";

    #[test]
    fn atmos_setting_should_be_read_from_its_section() {
        assert_eq!(atmos_enabled(SPA_FIXTURE), Some(true));
    }

    #[test]
    fn atmos_setting_should_read_false() {
        let text = SPA_FIXTURE.replace("    enabled: true", "    enabled: false");
        assert_eq!(atmos_enabled(&text), Some(false));
    }

    #[test]
    fn other_sections_enabled_keys_should_be_ignored() {
        let text =
            "audio:\n  crossfade:\n    enabled: true\nvisual:\n  atmos:\n    enabled: true\n";
        assert_eq!(atmos_enabled(text), None);
    }

    #[test]
    fn spatial_setting_should_be_read_next_to_client_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("spa-config.yml"), SPA_FIXTURE).unwrap();
        let client_options = dir.path().join("client-options.yml");
        assert_eq!(spatial_audio_enabled(&client_options), Some(true));
    }

    /// A realistic `client-options.yml` fixture: many unrelated keys, a quoted
    /// empty scalar, a flow sequence, an empty value, and the proxy keys. CRLF
    /// line endings, as `YamlDotNet` writes on Windows.
    const FIXTURE: &str = "UseEvergreenRuntime: true\r\n\
V8OptimizeForSize: \r\n\
LowMemoryMode: false\r\n\
WebView2RuntimeFlags: []\r\n\
LastGpuDriverVersion: 31.0.101.4502\r\n\
LastCleanedClientVersion: \"400.2\"\r\n\
WPFRenderingMode: software\r\n\
EnableGSMTC: true\r\n\
UseProxy: true\r\n\
ProxyServer: 127.0.0.1:8000\r\n\
ProxyPAC: \"\"\r\n";

    fn key_set(content: &str) -> Vec<String> {
        content
            .lines()
            .filter_map(|line| line.split_once(':').map(|(key, _)| key.to_owned()))
            .collect()
    }

    #[test]
    fn read_scalar_reads_plain_and_quoted_values() {
        assert_eq!(
            read_scalar(FIXTURE, "ProxyServer").as_deref(),
            Some("127.0.0.1:8000")
        );
        assert_eq!(read_scalar(FIXTURE, "ProxyPAC").as_deref(), Some(""));
        assert_eq!(
            read_scalar(FIXTURE, "LastCleanedClientVersion").as_deref(),
            Some("400.2")
        );
    }

    #[test]
    fn read_scalar_ignores_prefix_collisions() {
        // `UseProxy` must not match `UseEvergreenRuntime` etc.
        assert_eq!(read_scalar(FIXTURE, "UseProxy").as_deref(), Some("true"));
    }

    #[test]
    fn state_from_text_detects_pointing_at_us() {
        let state = state_from_text(FIXTURE, "127.0.0.1:8000");
        assert!(state.use_proxy);
        assert_eq!(state.proxy_server, "127.0.0.1:8000");
        assert!(state.points_at_us);
    }

    #[test]
    fn state_from_text_rejects_other_server() {
        let state = state_from_text(FIXTURE, "127.0.0.1:9999");
        assert!(!state.points_at_us);
    }

    #[test]
    fn disabling_flips_only_use_proxy_and_keeps_server() {
        let edited = edit_config(FIXTURE, false, "127.0.0.1:8000");
        assert_eq!(read_scalar(&edited, "UseProxy").as_deref(), Some("false"));
        // ProxyServer is preserved on disable.
        assert_eq!(
            read_scalar(&edited, "ProxyServer").as_deref(),
            Some("127.0.0.1:8000")
        );
    }

    #[test]
    fn enabling_sets_both_keys() {
        let off = edit_config(FIXTURE, false, "127.0.0.1:8000");
        let on = edit_config(&off, true, "127.0.0.1:7000");
        assert_eq!(read_scalar(&on, "UseProxy").as_deref(), Some("true"));
        assert_eq!(
            read_scalar(&on, "ProxyServer").as_deref(),
            Some("127.0.0.1:7000")
        );
    }

    #[test]
    fn edit_preserves_all_other_keys_and_their_formatting() {
        let edited = edit_config(FIXTURE, false, "127.0.0.1:8000");
        // No keys lost or added.
        assert_eq!(key_set(&edited), key_set(FIXTURE));
        // Every line except the single edited `UseProxy` line is byte-identical.
        for (before, after) in FIXTURE.lines().zip(edited.lines()) {
            if before.starts_with("UseProxy:") {
                assert_eq!(after, "UseProxy: false");
            } else {
                assert_eq!(before, after, "untouched line changed");
            }
        }
    }

    #[test]
    fn edit_preserves_crlf_line_endings() {
        let edited = edit_config(FIXTURE, false, "127.0.0.1:8000");
        assert_eq!(
            edited.matches("\r\n").count(),
            FIXTURE.matches("\r\n").count()
        );
        assert!(!edited.contains("\n\n"));
    }

    #[test]
    fn edit_preserves_quoted_empty_and_sequence_values() {
        let edited = edit_config(FIXTURE, true, "127.0.0.1:8000");
        assert!(edited.contains("ProxyPAC: \"\"\r\n"));
        assert!(edited.contains("WebView2RuntimeFlags: []\r\n"));
        assert!(edited.contains("V8OptimizeForSize: \r\n"));
        assert!(edited.contains("LastCleanedClientVersion: \"400.2\"\r\n"));
    }

    #[test]
    fn set_scalar_appends_missing_key_with_dominant_newline() {
        let without = "UseProxy: true\r\nProxyServer: 127.0.0.1:8000\r\n";
        let edited = set_scalar(without, "ProxyPAC", "");
        assert!(edited.ends_with("ProxyPAC: \r\n"));
        assert_eq!(edited.matches("\r\n").count(), 3);
    }

    #[test]
    fn set_scalar_handles_lf_only_documents() {
        let lf = "UseProxy: true\nProxyServer: 127.0.0.1:8000\n";
        let edited = set_scalar(lf, "UseProxy", "false");
        assert_eq!(edited, "UseProxy: false\nProxyServer: 127.0.0.1:8000\n");
    }

    #[test]
    fn set_proxy_at_writes_atomically_and_leaves_backups_untouched() {
        // Temp dir with the main config plus the rotating backup siblings.
        let dir = std::env::temp_dir().join(format!(
            "cider-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let main = dir.join("client-options.yml");
        let backup1 = dir.join("client-options.yml.backup1");
        let backup2 = dir.join("client-options.yml.backup2");
        std::fs::write(&main, FIXTURE).expect("write main");
        std::fs::write(&backup1, FIXTURE).expect("write backup1");
        std::fs::write(&backup2, FIXTURE).expect("write backup2");

        set_proxy_at(&main, false, "127.0.0.1:8000").expect("set proxy");

        let updated = std::fs::read_to_string(&main).expect("read main");
        assert_eq!(read_scalar(&updated, "UseProxy").as_deref(), Some("false"));
        assert_eq!(key_set(&updated), key_set(FIXTURE));
        // Backups are Cider-owned and must not be touched by our edit.
        assert_eq!(
            std::fs::read_to_string(&backup1).expect("read backup1"),
            FIXTURE
        );
        assert_eq!(
            std::fs::read_to_string(&backup2).expect("read backup2"),
            FIXTURE
        );
        // The temp file left no residue.
        assert!(!dir.join("client-options.yml.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_cider_process_should_match_case_insensitively() {
        for name in ["Cider.exe", "cider", "Cider"] {
            assert!(is_cider_process(OsStr::new(name), None), "{name}");
        }
    }

    #[test]
    fn is_cider_process_should_not_match_this_app() {
        assert!(!is_cider_process(OsStr::new("cider-offline.exe"), None));
        assert!(!is_cider_process(OsStr::new("Cider Offline"), None));
    }

    #[test]
    fn is_cider_process_should_match_by_exe_path() {
        assert!(is_cider_process(
            OsStr::new("AppRun"),
            Some(Path::new("/opt/Cider/cider"))
        ));
    }

    /// A fresh, empty directory for one test.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cider-{label}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn locate_should_prefer_chosen_existing_file() {
        let dir = scratch_dir("locate-chosen");
        let chosen = dir.join("my options.yml");
        std::fs::write(&chosen, FIXTURE).expect("write chosen");
        let default = dir.join("default.yml");
        std::fs::write(&default, FIXTURE).expect("write default");
        let found = locate_in(Some(&chosen), std::slice::from_ref(&default));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found.expect("located"), chosen);
    }

    #[test]
    fn locate_should_ignore_missing_chosen_file() {
        let dir = scratch_dir("locate-missing");
        let missing = dir.join("gone.yml");
        let found = locate_in(Some(&missing), &[]);
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(found, Err(CiderError::ConfigNotFound(_))));
    }

    #[test]
    fn locate_should_fall_back_to_existing_default() {
        let dir = scratch_dir("locate-default");
        let default = dir.join("default.yml");
        std::fs::write(&default, FIXTURE).expect("write default");
        let found = locate_in(None, &[dir.join("nope.yml"), default.clone()]);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found.expect("located"), default);
    }

    #[cfg(not(windows))]
    #[test]
    fn manual_guide_should_mention_proxy_flag() {
        assert!(manual_guide("127.0.0.1:8000").contains("--proxy-server=127.0.0.1:8000"));
    }

    #[test]
    fn manual_guide_should_show_configured_listen_address() {
        let guide = manual_guide("192.168.1.20:9123");
        assert!(guide.contains("192.168.1.20:9123"));
        assert!(!guide.contains("127.0.0.1:8000"));
    }

    #[test]
    fn read_state_reports_config_not_found() {
        let missing = std::env::temp_dir().join("cider-does-not-exist-xyz/client-options.yml");
        match read_state(&missing, "127.0.0.1:8000") {
            Err(CiderError::ConfigNotFound(_)) => {}
            other => panic!("expected ConfigNotFound, got {other:?}"),
        }
    }
}
