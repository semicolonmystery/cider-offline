//! Persistent user configuration and on-disk locations.

use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::Mode;

/// Auto-cache behaviour: nothing, immediate download, or deferred until the
/// user stops playing music for a while.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCacheMode {
    /// No auto-caching; only manually enqueued downloads run.
    #[default]
    Off,
    /// Auto-cache a song as soon as it plays.
    Instant,
    /// Enqueue auto-cache rows but hold them while playback is active; drain
    /// them after a brief idle window.
    Deferred,
}

impl AutoCacheMode {
    /// Stable numeric form for `AtomicU8` storage and UI binding.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Instant => 1,
            Self::Deferred => 2,
        }
    }

    /// Parses the numeric form; unknown values map to [`AutoCacheMode::Off`].
    #[must_use]
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Instant,
            2 => Self::Deferred,
            _ => Self::Off,
        }
    }
}

/// How much metadata is stored when the proxy observes a song.
///
/// `Full` fills every typed track column and keeps the complete resource JSON
/// (powering rich library / search / lyrics, at the cost of more rows and
/// optional extra catalog fetches). `Minimal` keeps only the core fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    /// Store the full attribute set and complete resource JSON.
    #[default]
    Full,
    /// Store only id, title, artist, album, duration and artwork.
    Minimal,
}

impl DetailLevel {
    /// Stable numeric form for `AtomicU8` storage and UI binding.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::Minimal => 1,
        }
    }

    /// Parses the numeric form; unknown values map to [`DetailLevel::Full`].
    #[must_use]
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Minimal,
            _ => Self::Full,
        }
    }
}

/// Which songs Cider's library Songs page lists (`/v1/me/library/songs`).
///
/// Applies online and offline. Only the Songs listing changes: lookups by
/// id, playlists, albums, search and playback are the same in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SongsPage {
    /// Exactly Apple's Songs tab: only the songs added to the library
    /// themselves (online Apple's own answer, offline the synced list).
    #[default]
    Apple,
    /// Every song of the library, including every song of every library
    /// playlist and album (answered locally, online too).
    All,
}

impl SongsPage {
    /// Stable numeric form for `AtomicU8` storage and UI binding.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Apple => 0,
            Self::All => 1,
        }
    }

    /// Parses the numeric form; unknown values map to [`SongsPage::Apple`].
    #[must_use]
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::All,
            _ => Self::Apple,
        }
    }
}

/// Which mixes of a song a download fetches.
///
/// Every Apple Music song has the stereo mix. Songs mastered in
/// Dolby Atmos also have a spatial stream: the Atmos mix rendered to
/// headphone stereo (binaural), which Cider plays when its spatial audio
/// setting is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadFormat {
    /// Only the stereo mix.
    #[default]
    Stereo,
    /// The spatial mix when the song has one, otherwise the stereo one.
    SpatialWhereAvailable,
    /// The stereo mix, plus the spatial one when the song has it.
    Both,
}

impl DownloadFormat {
    /// Stable numeric form for `AtomicU8` storage and UI binding.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Stereo => 0,
            Self::SpatialWhereAvailable => 1,
            Self::Both => 2,
        }
    }

    /// Parses the numeric form; unknown values map to
    /// [`DownloadFormat::Stereo`].
    #[must_use]
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::SpatialWhereAvailable,
            2 => Self::Both,
            _ => Self::Stereo,
        }
    }
}

/// What happens to unlinked downloads: downloaded files no album, playlist
/// or single-song download needs any more (see
/// [`crate::download_cleanup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnlinkedCleanup {
    /// Kept until the user clicks "Clean up now" in Settings.
    #[default]
    Manual,
    /// Deleted by a check every [`Config::unlinked_cleanup_hours`] hours.
    Automatic,
}

/// Default hours between automatic checks for unlinked downloads.
pub const DEFAULT_UNLINKED_CLEANUP_HOURS: u32 = 6;
/// Largest accepted number of hours between automatic checks (a week).
pub const MAX_UNLINKED_CLEANUP_HOURS: u32 = 168;

/// How the proxy presents the Apple Music library to Cider.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LibrarySettings {
    /// Which songs the library Songs page lists.
    pub songs_page: SongsPage,
}

/// Errors from loading or saving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The OS did not provide a home / application-data directory.
    #[error("could not determine the per-user application directories")]
    NoProjectDirs,
    /// Reading or writing a file failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// File that was being accessed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The configuration file is not valid TOML for [`Config`].
    #[error("invalid config file {path}: {source}")]
    Parse {
        /// Config file path.
        path: PathBuf,
        /// Parse error.
        #[source]
        source: toml::de::Error,
    },
    /// The configuration file parsed but holds a value the app cannot use.
    #[error("invalid config file {path}: {message}")]
    Invalid {
        /// Config file path.
        path: PathBuf,
        /// What is wrong and how to fix it.
        message: String,
    },
    /// The configuration could not be serialized.
    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> ConfigError + '_ {
    move |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// User configuration, persisted as `config.toml` in the config directory.
///
/// Unknown keys are ignored and missing keys take their defaults, so a file
/// written by another version of the app keeps loading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // user-facing toggles, not a state machine
pub struct Config {
    /// Listening socket of the proxy.
    pub proxy: ProxySettings,
    /// Mode the proxy starts in.
    pub mode: Mode,
    /// Whether Apple-host request/response pairs are written to `captures/`.
    pub capture: bool,
    /// How the proxy auto-caches songs that play through it: off, download
    /// immediately, or defer until playback has been idle for a short window.
    pub auto_cache: AutoCacheMode,
    /// How much metadata is stored for observed songs (Full or Minimal).
    pub detail_level: DetailLevel,
    /// How many downloads run at once (1 to [`crate::MAX_CONCURRENCY`]).
    /// `0`, the value until the user picks one in Settings, means
    /// [`crate::DEFAULT_CONCURRENCY`].
    pub download_concurrency: u8,
    /// Which mixes of a song downloads fetch (stereo, spatial where
    /// available, or both).
    pub download_format: DownloadFormat,
    /// "Play the other downloaded mix": when Cider asks for a mix of a
    /// song that isn't downloaded (stereo or spatial) but the other one is,
    /// play the downloaded one instead of streaming from Apple. Offline this
    /// always happens.
    pub play_other_downloaded_mix: bool,
    /// "Remove lyrics when removing downloads": "Remove download(s)" also
    /// deletes the lyrics the app fetched for those songs. Lyrics the user
    /// imported (custom lyrics) are never removed this way.
    pub remove_lyrics_with_downloads: bool,
    /// Unlinked downloads: kept until cleaned up by hand ("manual") or
    /// deleted automatically ("automatic").
    pub unlinked_downloads: UnlinkedCleanup,
    /// Hours between automatic checks for unlinked downloads (1 to
    /// [`MAX_UNLINKED_CLEANUP_HOURS`]). `0`, the value until the user picks
    /// one, means [`DEFAULT_UNLINKED_CLEANUP_HOURS`].
    pub unlinked_cleanup_hours: u32,
    /// When importing / downloading a track that already has audio, replace it
    /// instead of skipping.
    pub overwrite_audio: bool,

    /// When fetching lyrics in bulk (a collection, after a download), look
    /// songs that already have lyrics up again and replace them. Apple
    /// lyrics are always kept; a single song's "Fetch lyrics" always looks up.
    pub overwrite_lyrics: bool,
    /// Lyrics providers, their order and failure handling.
    pub lyrics: crate::lyrics::LyricsSettings,
    /// How the library is presented to Cider (`[library]`).
    pub library: LibrarySettings,
    /// Launch the app when the user logs in.
    pub start_with_os: bool,
    /// Start hidden in the system tray.
    pub start_minimized: bool,
    /// Apple Music reachability probing.
    pub connectivity: ConnectivitySettings,
    /// Cider config file chosen by the user ("Choose Cider config file...").
    pub cider_config_path: Option<PathBuf>,
    /// The one-time "no system tray" hint was dismissed.
    pub tray_hint_dismissed: bool,
}

/// Address the proxy listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    /// Bind address. Defaults to loopback so the proxy is not exposed to the network.
    pub bind_address: IpAddr,
    /// TCP port, `1..=65535`. `config.toml` may not set `0`, because Cider
    /// has to be pointed at a fixed address; only the app's own checks and
    /// tests set `0` in code to get a free port.
    pub port: u16,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8000,
        }
    }
}

/// Settings for the connectivity monitor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectivitySettings {
    /// Whether the monitor runs. When disabled the proxy assumes it is online.
    pub enabled: bool,
    /// URL probed with a `HEAD` request. Any HTTP response counts as online.
    pub probe_url: String,
    /// Seconds between probes.
    pub interval_secs: u64,
    /// Per-probe timeout in seconds.
    pub timeout_secs: u64,
    /// Treat Apple Music as unreachable regardless of the probe, so every
    /// intercepted request is answered by the offline responder.
    pub force_offline: bool,
}

impl Default for ConnectivitySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            probe_url: "https://amp-api.music.apple.com/".to_owned(),
            interval_secs: 15,
            timeout_secs: 5,
            force_offline: false,
        }
    }
}

impl ConnectivitySettings {
    /// Probe interval (at least one second).
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.max(1))
    }

    /// Probe timeout (at least one second).
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }
}

impl Config {
    /// The effective simultaneous-download limit: the configured value
    /// clamped to `1..=`[`crate::MAX_CONCURRENCY`], or
    /// [`crate::DEFAULT_CONCURRENCY`] when unset.
    pub fn download_concurrency(&self) -> usize {
        match self.download_concurrency {
            0 => crate::DEFAULT_CONCURRENCY,
            n => crate::clamp_concurrency(usize::from(n)),
        }
    }

    /// Hours between automatic checks for unlinked downloads, clamped to
    /// `1..=`[`MAX_UNLINKED_CLEANUP_HOURS`] ([`DEFAULT_UNLINKED_CLEANUP_HOURS`]
    /// when unset).
    pub fn unlinked_cleanup_hours(&self) -> u32 {
        match self.unlinked_cleanup_hours {
            0 => DEFAULT_UNLINKED_CLEANUP_HOURS,
            n => n.min(MAX_UNLINKED_CLEANUP_HOURS),
        }
    }

    /// Socket address the proxy binds to.
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.proxy.bind_address, self.proxy.port)
    }

    /// Parses a config from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] for invalid TOML and
    /// [`ConfigError::Invalid`] for a value the app cannot use (such as
    /// `proxy.port = 0`); `origin` is used in the message.
    pub fn from_toml(text: &str, origin: &Path) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_path_buf(),
            source,
        })?;
        config.validate(origin)?;
        Ok(config)
    }

    /// Rejects parsed values the app cannot run with.
    fn validate(&self, origin: &Path) -> Result<(), ConfigError> {
        if self.proxy.port == 0 {
            return Err(ConfigError::Invalid {
                path: origin.to_path_buf(),
                message: "proxy.port must be between 1 and 65535 (0 is not allowed)".to_owned(),
            });
        }
        Ok(())
    }

    /// Serializes this config as TOML.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Serialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Loads the config at `path`.
    ///
    /// # Errors
    ///
    /// Returns I/O or parse errors.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(io_err(path))?;
        Self::from_toml(&text, path)
    }

    /// Loads the config at `path`, or returns the defaults when the file does
    /// not exist yet.
    ///
    /// # Errors
    ///
    /// Returns I/O errors other than "not found", and parse errors.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(io_err(path)(e)),
        }
    }

    /// Writes the config to `path` atomically (temp file + rename), creating
    /// parent directories as needed.
    ///
    /// # Errors
    ///
    /// Returns I/O or serialization errors.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = self.to_toml()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        let tmp = path.with_extension("toml.tmp");
        let mut file = fs::File::create(&tmp).map_err(io_err(&tmp))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(io_err(&tmp))?;
        drop(file);
        fs::rename(&tmp, path).map_err(io_err(path))
    }
}

/// Name of the folder that switches the app into portable mode when it sits
/// next to the program.
pub const PORTABLE_DIR: &str = "data";

/// Empty file the portable archives ship inside `data/` so unzip tools keep
/// the otherwise empty folder.
pub const PORTABLE_PLACEHOLDER: &str = ".keep";

/// Per-user directories used by the app.
///
/// Installed layout (`ProjectDirs`): the config directory holds `config.toml`;
/// the data directory holds `store.sqlite`, `media/`, `lyrics/`, `ca/`,
/// `captures/`, `instance.lock` and the optional `device.wvd`; the cache
/// directory holds `artwork/` and `logs/`.
///
/// Portable layout (a `data` folder next to the program): everything lives in
/// that folder, with config in `config/`, the data files directly inside and
/// the cache in `cache/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    portable_root: Option<PathBuf>,
}

/// Folder that decides portable mode: where the `data` folder would live.
///
/// This is the parent of the `AppImage` on Linux when it is known, the folder
/// containing the `.app` bundle on macOS, and otherwise the executable's folder.
pub fn portable_base(exe: &Path, appimage: Option<&Path>) -> Option<PathBuf> {
    if cfg!(target_os = "linux")
        && let Some(parent) = appimage.and_then(Path::parent)
    {
        return Some(parent.to_path_buf());
    }
    if cfg!(target_os = "macos")
        && let Some(bundle) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")))
    {
        return bundle.parent().map(Path::to_path_buf);
    }
    exe.parent().map(Path::to_path_buf)
}

impl AppPaths {
    /// Qualifier passed to [`ProjectDirs::from`].
    pub const QUALIFIER: &'static str = "sh";
    /// Organization passed to [`ProjectDirs::from`].
    pub const ORGANIZATION: &'static str = "cider-offline";
    /// Application name passed to [`ProjectDirs::from`].
    pub const APPLICATION: &'static str = "CiderOffline";

    /// Resolves the directories for this run from the current executable (and
    /// `$APPIMAGE` on Linux): portable if a `data` folder sits next to the
    /// program, otherwise the standard per-user folders.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoProjectDirs`] if no home directory is known,
    /// or [`ConfigError::Io`] if the executable path cannot be determined.
    pub fn discover() -> Result<Self, ConfigError> {
        let exe = std::env::current_exe().map_err(|source| ConfigError::Io {
            path: PathBuf::from("current_exe"),
            source,
        })?;
        let appimage = if cfg!(target_os = "linux") {
            std::env::var_os("APPIMAGE").map(PathBuf::from)
        } else {
            None
        };
        Self::discover_from(&exe, appimage.as_deref())
    }

    /// [`AppPaths::discover`] with the executable and `AppImage` path given.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoProjectDirs`] if no home directory is known.
    pub fn discover_from(exe: &Path, appimage: Option<&Path>) -> Result<Self, ConfigError> {
        if let Some(base) = portable_base(exe, appimage) {
            let root = base.join(PORTABLE_DIR);
            if root.is_dir() {
                return Ok(Self::under(&root));
            }
        }
        let dirs = ProjectDirs::from(Self::QUALIFIER, Self::ORGANIZATION, Self::APPLICATION)
            .ok_or(ConfigError::NoProjectDirs)?;
        Ok(Self::new(
            dirs.config_dir(),
            dirs.data_local_dir(),
            dirs.cache_dir(),
        ))
    }

    /// Uses explicit directories (installed layout).
    pub fn new(
        config_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config_dir: config_dir.into(),
            data_dir: data_dir.into(),
            cache_dir: cache_dir.into(),
            portable_root: None,
        }
    }

    /// Portable layout rooted at `root`: config in `root/config`, data in
    /// `root`, cache in `root/cache`.
    pub fn under(root: &Path) -> Self {
        Self {
            config_dir: root.join("config"),
            data_dir: root.to_path_buf(),
            cache_dir: root.join("cache"),
            portable_root: Some(root.to_path_buf()),
        }
    }

    /// Creates the config and data directories and the standard data subdirectories.
    ///
    /// # Errors
    ///
    /// Returns the first I/O error encountered.
    pub fn ensure_dirs(&self) -> Result<(), ConfigError> {
        for dir in [
            self.config_dir.clone(),
            self.data_dir.clone(),
            self.cache_dir.clone(),
            self.media_dir(),
            self.lyrics_dir(),
            self.artwork_dir(),
        ] {
            fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        }
        Ok(())
    }

    /// Config directory.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// `config.toml` path.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// SQLite database path.
    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join("store.sqlite")
    }

    /// Directory holding the local CA certificate and key.
    pub fn ca_dir(&self) -> PathBuf {
        self.data_dir.join("ca")
    }

    /// Directory for capture JSON files.
    pub fn captures_dir(&self) -> PathBuf {
        self.data_dir.join("captures")
    }

    /// Directory for local audio files.
    pub fn media_dir(&self) -> PathBuf {
        self.data_dir.join("media")
    }

    /// Directory for local lyrics (TTML).
    pub fn lyrics_dir(&self) -> PathBuf {
        self.data_dir.join("lyrics")
    }

    /// Directory for cached artwork.
    pub fn artwork_dir(&self) -> PathBuf {
        self.cache_dir.join("artwork")
    }

    /// Cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Directory for log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.cache_dir.join("logs")
    }

    /// Single-instance lock file.
    pub fn instance_lock_file(&self) -> PathBuf {
        self.data_dir.join("instance.lock")
    }

    /// Optional user-supplied Widevine device override.
    pub fn device_override_file(&self) -> PathBuf {
        self.data_dir.join("device.wvd")
    }

    /// Portable root folder, when running portable.
    pub fn portable_root(&self) -> Option<&Path> {
        self.portable_root.as_deref()
    }

    /// SQLite's sidecar files next to [`Self::database_file`] (write-ahead
    /// log, shared memory, rollback journal).
    pub fn database_sidecar_files(&self) -> [PathBuf; 3] {
        ["-wal", "-shm", "-journal"].map(|suffix| {
            let mut name = self.database_file().into_os_string();
            name.push(suffix);
            PathBuf::from(name)
        })
    }

    /// Every file and folder the app creates: the config and cache folders
    /// and each app entry in the data folder. In portable mode these are all
    /// inside the portable root, plus the [`PORTABLE_PLACEHOLDER`] the
    /// archives ship; anything else in that folder belongs to the user.
    pub fn owned_entries(&self) -> Vec<PathBuf> {
        let mut entries = vec![
            self.config_dir.clone(),
            self.cache_dir.clone(),
            self.database_file(),
        ];
        entries.extend(self.database_sidecar_files());
        entries.extend([
            self.media_dir(),
            self.lyrics_dir(),
            self.ca_dir(),
            self.captures_dir(),
            self.instance_lock_file(),
            self.device_override_file(),
        ]);
        if let Some(root) = &self.portable_root {
            entries.push(root.join(PORTABLE_PLACEHOLDER));
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_should_put_everything_inside_root() {
        let root = Path::new("/p/data");
        let p = AppPaths::under(root);
        assert_eq!(p.data_dir(), root);
        assert_eq!(p.config_dir(), root.join("config"));
        assert_eq!(p.cache_dir(), root.join("cache"));
        assert_eq!(p.artwork_dir(), root.join("cache").join("artwork"));
        assert_eq!(p.portable_root(), Some(root));
    }

    #[test]
    fn owned_entries_should_be_direct_children_of_portable_root() {
        let root = Path::new("/p/data");

        let entries = AppPaths::under(root).owned_entries();

        assert!(
            entries.iter().all(|entry| entry.parent() == Some(root)),
            "{entries:?}"
        );
    }

    #[test]
    fn database_sidecar_files_should_sit_next_to_the_database() {
        let p = AppPaths::under(Path::new("/p/data"));

        assert_eq!(
            p.database_sidecar_files(),
            [
                "store.sqlite-wal",
                "store.sqlite-shm",
                "store.sqlite-journal"
            ]
            .map(|name| Path::new("/p/data").join(name))
        );
    }

    #[test]
    #[cfg(windows)]
    fn portable_base_should_use_exe_folder() {
        let exe = Path::new(r"C:\Apps\Cider Offline 雪\cider-offline.exe");
        assert_eq!(
            portable_base(exe, None),
            Some(PathBuf::from(r"C:\Apps\Cider Offline 雪"))
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn portable_base_should_use_appimage_parent() {
        let exe = Path::new("/tmp/.mount_x/usr/bin/cider-offline");
        let img = Path::new("/home/u/My Apps/Cider Offline.AppImage");
        assert_eq!(
            portable_base(exe, Some(img)),
            Some(PathBuf::from("/home/u/My Apps"))
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn portable_base_should_use_folder_containing_app() {
        let exe = Path::new("/Users/u/Tools/Cider Offline.app/Contents/MacOS/cider-offline");
        assert_eq!(
            portable_base(exe, None),
            Some(PathBuf::from("/Users/u/Tools"))
        );
    }

    #[test]
    fn discover_from_should_pick_portable_when_data_folder_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(PORTABLE_DIR)).unwrap();
        let exe = dir.path().join("cider-offline.exe");
        let p = AppPaths::discover_from(&exe, None).unwrap();
        assert_eq!(
            p.portable_root(),
            Some(dir.path().join(PORTABLE_DIR).as_path())
        );
    }

    #[test]
    fn discover_from_should_use_os_folders_without_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let p = AppPaths::discover_from(&dir.path().join("app.exe"), None).unwrap();
        assert!(p.portable_root().is_none());
        assert!(p.artwork_dir().starts_with(p.cache_dir()));
    }

    #[test]
    fn config_should_roundtrip_cider_config_path_and_hint() {
        let config = Config {
            cider_config_path: Some(PathBuf::from("/x/client-options.yml")),
            tray_hint_dismissed: true,
            ..Config::default()
        };
        assert_eq!(
            Config::from_toml(&config.to_toml().unwrap(), Path::new("x")).unwrap(),
            config
        );
    }

    #[test]
    fn default_config_should_listen_on_loopback_port_8000() {
        assert_eq!(
            Config::default().listen_addr(),
            "127.0.0.1:8000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn config_should_roundtrip_through_toml() {
        let config = Config {
            mode: Mode::Passthrough,
            capture: true,
            proxy: ProxySettings {
                port: 9123,
                ..ProxySettings::default()
            },
            ..Config::default()
        };
        let text = config.to_toml().unwrap();
        assert_eq!(Config::from_toml(&text, Path::new("x")).unwrap(), config);
    }

    #[test]
    fn config_should_roundtrip_through_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let config = Config {
            start_minimized: true,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn partial_config_should_fill_defaults() {
        let config = Config::from_toml("mode = \"passthrough\"\n", Path::new("x")).unwrap();
        assert_eq!(config.proxy, ProxySettings::default());
    }

    #[test]
    fn library_settings_should_default_to_full_detail_and_no_overwrite() {
        let config = Config::default();
        assert_eq!(config.detail_level, DetailLevel::Full);
        assert!(!config.overwrite_audio && !config.overwrite_lyrics);
    }

    #[test]
    fn detail_level_should_roundtrip_through_toml() {
        let config = Config {
            detail_level: DetailLevel::Minimal,
            overwrite_audio: true,
            ..Config::default()
        };
        let text = config.to_toml().unwrap();
        assert_eq!(Config::from_toml(&text, Path::new("x")).unwrap(), config);
    }

    #[test]
    fn download_format_should_default_to_stereo() {
        assert_eq!(Config::default().download_format, DownloadFormat::Stereo);
    }

    #[test]
    fn download_format_should_parse_snake_case_names() {
        let config = Config::from_toml(
            "download_format = \"spatial_where_available\"\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(
            config.download_format,
            DownloadFormat::SpatialWhereAvailable
        );
    }

    #[test]
    fn download_format_should_roundtrip_through_toml() {
        let config = Config {
            download_format: DownloadFormat::Both,
            ..Config::default()
        };
        let text = config.to_toml().unwrap();
        assert_eq!(Config::from_toml(&text, Path::new("x")).unwrap(), config);
    }

    #[test]
    fn unlinked_downloads_should_default_to_manual_every_six_hours() {
        let config = Config::default();
        assert_eq!(
            (config.unlinked_downloads, config.unlinked_cleanup_hours()),
            (UnlinkedCleanup::Manual, 6)
        );
    }

    #[test]
    fn unlinked_downloads_should_parse_from_toml() {
        let config = Config::from_toml(
            "unlinked_downloads = \"automatic\"\nunlinked_cleanup_hours = 12\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(
            (config.unlinked_downloads, config.unlinked_cleanup_hours()),
            (UnlinkedCleanup::Automatic, 12)
        );
    }

    #[test]
    fn unlinked_cleanup_hours_should_be_capped_at_a_week() {
        let config = Config {
            unlinked_cleanup_hours: 10_000,
            ..Config::default()
        };
        assert_eq!(config.unlinked_cleanup_hours(), MAX_UNLINKED_CLEANUP_HOURS);
    }

    #[test]
    fn remove_lyrics_with_downloads_should_default_off() {
        assert!(!Config::default().remove_lyrics_with_downloads);
    }

    #[test]
    fn remove_lyrics_with_downloads_should_parse_from_toml() {
        let config =
            Config::from_toml("remove_lyrics_with_downloads = true\n", Path::new("x")).unwrap();
        assert!(config.remove_lyrics_with_downloads);
    }

    #[test]
    fn play_other_downloaded_mix_should_default_off() {
        assert!(!Config::default().play_other_downloaded_mix);
    }

    #[test]
    fn play_other_downloaded_mix_should_parse_from_toml() {
        let config =
            Config::from_toml("play_other_downloaded_mix = true\n", Path::new("x")).unwrap();
        assert!(config.play_other_downloaded_mix);
    }

    #[test]
    fn download_format_should_roundtrip_through_u8() {
        for format in [
            DownloadFormat::Stereo,
            DownloadFormat::SpatialWhereAvailable,
            DownloadFormat::Both,
        ] {
            assert_eq!(DownloadFormat::from_u8(format.as_u8()), format);
        }
    }

    #[test]
    fn load_or_default_should_return_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_or_default(&dir.path().join("missing.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn invalid_config_should_report_parse_error() {
        let err = Config::from_toml("mode = 5", Path::new("x")).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn port_zero_should_be_rejected() {
        let err = Config::from_toml(
            "[proxy]
port = 0
",
            Path::new("x"),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn nonzero_port_should_be_accepted() {
        let config = Config::from_toml(
            "[proxy]
port = 9123
",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(config.proxy.port, 9123);
    }
}
