//! Logging to stdout and to a daily log file in the cache folder.

use std::path::PathBuf;

use cider_offline_core::AppPaths;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Log file name prefix; the date and `.log` are appended per day.
const FILE_PREFIX: &str = "cider-offline";
/// Log file name suffix.
const FILE_SUFFIX: &str = "log";
/// Daily log files kept before the oldest is deleted.
const KEEP_FILES: usize = 7;
/// Filter used when `RUST_LOG` is unset or invalid.
const DEFAULT_FILTER: &str = "info,hudsucker=warn";

/// Keeps the background log-file writer alive; buffered lines are flushed
/// when it is dropped, so hold it until the end of `main`.
pub struct LogGuard(#[expect(dead_code, reason = "held only for its Drop")] WorkerGuard);

/// Folder holding the log files.
pub fn log_dir(paths: &AppPaths) -> PathBuf {
    paths.logs_dir()
}

/// Installs the global subscriber: stdout, plus a daily rotated file in the
/// logs folder (keeping the last [`KEEP_FILES`]) when `paths` is known and the
/// folder can be created. Returns the guard of the file writer, if any.
pub fn init(paths: Option<&AppPaths>) -> Option<LogGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let mut file_error = None;
    let file = paths.and_then(|paths| match open_file(&log_dir(paths)) {
        Ok(appender) => Some(appender),
        Err(error) => {
            file_error = Some(error);
            None
        }
    });
    // Colours only on stdout; the file gets plain text.
    let (file_layer, guard) = file.map_or((None, None), |appender| {
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false);
        (Some(layer), Some(LogGuard(guard)))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
        .init();
    if let Some(error) = file_error {
        tracing::warn!(%error, "file logging unavailable; logging to stdout only");
    }
    guard
}

/// Creates the logs folder and the rolling appender in it.
fn open_file(dir: &std::path::Path) -> anyhow::Result<RollingFileAppender> {
    std::fs::create_dir_all(dir)?;
    Ok(RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .max_log_files(KEEP_FILES)
        .build(dir)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use cider_offline_core::AppPaths;

    #[test]
    fn log_dir_should_be_under_cache() {
        let paths = AppPaths::new("/c/config", "/c/data", "/c/cache");

        assert!(log_dir(&paths).starts_with(paths.cache_dir()));
    }
}
