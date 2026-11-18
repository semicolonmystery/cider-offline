//! "Remove all data": the Settings button and the uninstaller flags.
//!
//! Two halves, so the in-app flow can run them at different times:
//! [`remove_integration`] undoes what the app changed elsewhere on the system
//! (trusted root CA, start-at-login entry, Cider's proxy setting), and
//! [`remove_files`] deletes the app's own folders. The app runs the second
//! half only after the proxy has stopped and the instance lock is released,
//! so no file it deletes is still open (Windows refuses to delete those).
//!
//! Only the app's own folders are deleted: files the user picked elsewhere
//! (linked audio, imported lyrics) are never touched.

use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::bail;
use cider_offline_core::{AppPaths, Config};
use cider_offline_trust::{CertificateAuthority, TrustStore as _, system_trust_store};

use crate::cider;
use crate::instance;
use crate::settings::Autostart;

/// Removes the system integration and every app file, then exits.
pub const REMOVE_ALL_FLAG: &str = "--remove-all-data";
/// Removes only the system integration, then exits (the uninstaller always runs this).
pub const UNINSTALL_CLEANUP_FLAG: &str = "--uninstall-cleanup";
/// With one of the flags above: print the planned steps and change nothing.
pub const DRY_RUN_FLAG: &str = "--dry-run";

/// Attempts at deleting one folder. A file closed just before (the log file
/// after its writer stopped) can stay "delete pending" for a moment on Windows.
const REMOVE_ATTEMPTS: u32 = 3;
/// Pause between [`REMOVE_ATTEMPTS`].
const RETRY_DELAY: Duration = Duration::from_millis(150);

/// Start of the name of the plain-text report written to the OS temp folder
/// when the in-app file removal fails (see [`report_removal_failures`]).
/// Each report gets a new, uniquely named file.
pub const REMOVAL_REPORT_PREFIX: &str = "cider-offline-removal-report-";

/// Start of the name of the file a cleanup flag run writes its steps to in
/// the OS temp folder: on Windows the app has no console to print them to.
pub const CLEANUP_LOG_PREFIX: &str = "cider-offline-cleanup-";

/// How long a cleanup flag run waits for a quitting app to release the
/// instance lock (the Windows uninstaller kills the app, then waits only
/// briefly).
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// Pause between instance lock checks within [`LOCK_WAIT`].
const LOCK_POLL: Duration = Duration::from_millis(250);

/// What a [`Step`] acts on, so callers can say what a failure leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// The trusted root CA.
    Certificate,
    /// The start-at-login entry.
    Autostart,
    /// Cider's proxy setting.
    Cider,
    /// One of the app's files or folders.
    File,
}

impl StepKind {
    /// What stays on the system when a step of this kind fails, for the
    /// "Some steps failed" dialog. `None` for files (the dialog lists them).
    pub fn leftover(self) -> Option<&'static str> {
        match self {
            Self::Certificate => Some(
                "The proxy's certificate stays trusted by the system. Its \"ca\" folder is \
                 kept, so the uninstaller (or running the app with --uninstall-cleanup) can \
                 still remove it.",
            ),
            Self::Autostart => Some("The start-at-login entry stays."),
            Self::Cider => Some(
                "Cider's proxy setting stays on, so Cider keeps using this proxy address \
                 until you turn it off.",
            ),
            Self::File => None,
        }
    }
}

/// Whether [`remove_files`] deletes the CA folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaFolder {
    /// Delete it with everything else.
    Remove,
    /// Keep it: the certificate is still trusted, and a later
    /// [`UNINSTALL_CLEANUP_FLAG`] run needs it to remove the certificate.
    Keep,
}

/// One cleanup action and how it went.
#[derive(Debug)]
pub struct Step {
    /// What was done (or, in a dry run, would be done).
    pub what: String,
    /// `Err` holds a human-readable reason.
    pub result: Result<(), String>,
    /// What the step acts on.
    pub kind: StepKind,
}

impl Step {
    fn ok(what: impl Into<String>) -> Self {
        Self::new(what, Ok(()))
    }

    fn new(what: impl Into<String>, result: Result<(), String>) -> Self {
        Self {
            what: what.into(),
            result,
            kind: StepKind::File,
        }
    }

    fn of(self, kind: StepKind) -> Self {
        Self { kind, ..self }
    }
}

/// The steps of one cleanup run.
#[derive(Debug, Default)]
pub struct Report {
    /// Every step, in the order it ran.
    pub steps: Vec<Step>,
}

impl Report {
    /// Whether every step succeeded.
    pub fn is_ok(&self) -> bool {
        self.steps.iter().all(|step| step.result.is_ok())
    }

    /// The failed steps.
    pub fn failures(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|step| step.result.is_err())
    }

    /// One line per failed step, for the Settings page.
    pub fn failure_summary(&self) -> String {
        self.failures()
            .map(|step| match &step.result {
                Err(error) => format!("{}: {error}", step.what),
                Ok(()) => step.what.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What the failed integration steps leave behind, one sentence each.
    pub fn leftovers(&self) -> Vec<&'static str> {
        self.failures()
            .filter_map(|step| step.kind.leftover())
            .collect()
    }

    /// [`CaFolder::Keep`] when removing the certificate failed, so it can
    /// still be removed later.
    pub fn ca_folder(&self) -> CaFolder {
        if self
            .failures()
            .any(|step| step.kind == StepKind::Certificate)
        {
            CaFolder::Keep
        } else {
            CaFolder::Remove
        }
    }

    /// Logs every step.
    pub fn log(&self) {
        for step in &self.steps {
            match &step.result {
                Ok(()) => tracing::info!(step = %step.what, "cleanup step done"),
                Err(error) => tracing::error!(step = %step.what, %error, "cleanup step failed"),
            }
        }
    }

    fn push(&mut self, step: Step) {
        self.steps.push(step);
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.steps {
            match &step.result {
                Ok(()) => writeln!(f, "ok      {}", step.what)?,
                Err(error) => writeln!(f, "FAILED  {}: {error}", step.what)?,
            }
        }
        Ok(())
    }
}

/// Undoes the app's changes outside its folders: removes the CA from the
/// trust store (skipped when no CA was ever created), turns off start at
/// login, and turns off Cider's proxy setting when it points at this proxy.
///
/// Blocking: may show OS prompts and stop and relaunch Cider.
pub fn remove_integration(paths: &AppPaths, config: &Config, dry_run: bool) -> Report {
    let mut report = Report::default();
    report.push(remove_certificate(&paths.ca_dir(), dry_run).of(StepKind::Certificate));
    report.push(disable_autostart(dry_run).of(StepKind::Autostart));
    report.push(revert_cider(config, dry_run).of(StepKind::Cider));
    report
}

fn remove_certificate(ca_dir: &Path, dry_run: bool) -> Step {
    const WHAT: &str = "Remove the proxy's certificate from the trust store";
    match CertificateAuthority::load_existing(ca_dir) {
        Ok(None) => Step::ok(format!("{WHAT} (no certificate)")),
        Ok(Some(_)) if dry_run => Step::ok(WHAT),
        Ok(Some(ca)) => Step::new(
            WHAT,
            system_trust_store()
                .uninstall(&ca)
                .map_err(|error| error.to_string()),
        ),
        Err(error) => Step::new(WHAT, Err(error.to_string())),
    }
}

fn disable_autostart(dry_run: bool) -> Step {
    const WHAT: &str = "Turn off start at login";
    let Some(autostart) = Autostart::for_current_exe() else {
        return Step::ok(format!("{WHAT} (not supported here)"));
    };
    autostart_step(autostart.is_enabled(), dry_run, || {
        autostart.set_enabled(false)
    })
}

/// The [`disable_autostart`] step for a login entry whose state read as
/// `enabled`. A failed read is a failed step: the entry may still be there.
fn autostart_step(
    enabled: Result<bool, String>,
    dry_run: bool,
    disable: impl FnOnce() -> Result<(), String>,
) -> Step {
    const WHAT: &str = "Turn off start at login";
    match enabled {
        Err(error) => Step::new(
            WHAT,
            Err(format!("could not read the login entry: {error}")),
        ),
        Ok(false) => Step::ok(format!("{WHAT} (not enabled)")),
        Ok(true) if dry_run => Step::ok(WHAT),
        Ok(true) => Step::new(WHAT, disable()),
    }
}

/// Whether Cider's proxy setting is on and points at this proxy, so turning
/// it off is ours to do.
fn should_revert(state: &cider::CiderProxyState) -> bool {
    state.use_proxy && state.points_at_us
}

fn revert_cider(config: &Config, dry_run: bool) -> Step {
    const WHAT: &str = "Turn off Cider's proxy setting";
    if cider::SUPPORT != cider::Support::Automatic {
        return Step::ok(format!("{WHAT} (not managed on this OS)"));
    }
    let path = match cider::locate(config.cider_config_path.as_deref()) {
        Ok(path) => path,
        Err(cider::CiderError::ConfigNotFound(_)) => {
            return Step::ok(format!("{WHAT} (Cider's config not found)"));
        }
        Err(error) => return Step::new(WHAT, Err(error.to_string())),
    };
    let state = match cider::read_state(&path, &config.listen_addr().to_string()) {
        Ok(state) => state,
        Err(error) => return Step::new(WHAT, Err(error.to_string())),
    };
    if !should_revert(&state) {
        return Step::ok(format!("{WHAT} (not using this proxy)"));
    }
    if dry_run {
        return Step::ok(WHAT);
    }
    Step::new(
        WHAT,
        cider::apply_toggle(&path, false, &state.proxy_server)
            .map(|_| ())
            .map_err(|error| error.to_string()),
    )
}

/// Deletes the app's files.
///
/// Installed: the config, data and cache folders, then their
/// `cider-offline/<app>` parents when those are empty (Windows layout).
/// Portable: only the app's own entries inside the portable root
/// ([`AppPaths::owned_entries`]). Any other file or folder there belongs to
/// the user (a `data` folder next to the program may be one they made), and
/// the root itself stays so the next start is portable again.
/// Missing entries are not failures.
/// [`CaFolder::Keep`] spares the CA folder (and the folders holding it).
///
/// Call only after the proxy stopped and the log file writer was dropped.
pub fn remove_files(paths: &AppPaths, ca: CaFolder, dry_run: bool) -> Report {
    let ca_dir = paths.ca_dir();
    let keep = (ca == CaFolder::Keep).then_some(ca_dir.as_path());
    let mut report = Report::default();
    if paths.portable_root().is_some() {
        for entry in paths.owned_entries() {
            // Listing absent entries would only bury the real steps.
            if !matches!(std::fs::symlink_metadata(&entry),
                Err(error) if error.kind() == io::ErrorKind::NotFound)
            {
                remove_tree(&entry, keep, dry_run, &mut report);
            }
        }
        return report;
    }
    let mut removed: Vec<&Path> = Vec::new();
    for dir in [paths.config_dir(), paths.data_dir(), paths.cache_dir()] {
        if removed.iter().any(|done| dir.starts_with(done)) {
            continue;
        }
        remove_tree(dir, keep, dry_run, &mut report);
        removed.push(dir);
    }
    for parent in empty_parent_candidates(&removed) {
        if let Some(step) = remove_empty_dir(&parent, dry_run) {
            report.push(step);
        }
    }
    report
}

/// Where [`remove_files`] deletes, for the confirmation dialog: one line per
/// folder.
pub fn removal_locations(paths: &AppPaths) -> String {
    if let Some(root) = paths.portable_root() {
        return format!(
            "The app's own files and folders inside {} (anything else you keep there stays).",
            root.display()
        );
    }
    let mut folders: Vec<&Path> = Vec::new();
    for dir in [paths.config_dir(), paths.data_dir(), paths.cache_dir()] {
        if !folders.iter().any(|done| dir.starts_with(done)) {
            folders.push(dir);
        }
    }
    folders
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Deletes `path`, except `keep` and the folders leading to it.
fn remove_tree(path: &Path, keep: Option<&Path>, dry_run: bool, report: &mut Report) {
    match keep {
        Some(keep) if keep == path => report.push(Step::ok(format!(
            "Keep {} (the certificate is still trusted)",
            path.display()
        ))),
        Some(keep)
            if keep.starts_with(path)
                && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) =>
        {
            empty_dir(path, Some(keep), dry_run, report);
        }
        _ => report.push(remove_path(path, dry_run)),
    }
}

/// Deletes every entry inside `root` (except `keep`), keeping `root` itself.
fn empty_dir(root: &Path, keep: Option<&Path>, dry_run: bool, report: &mut Report) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            report.push(Step::new(
                format!("Read {}", root.display()),
                Err(error.to_string()),
            ));
            return;
        }
    };
    let mut targets: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => targets.push(entry.path()),
            Err(error) => report.push(Step::new(
                format!("Read {}", root.display()),
                Err(error.to_string()),
            )),
        }
    }
    targets.sort();
    for target in targets {
        remove_tree(&target, keep, dry_run, report);
    }
}

/// Deletes a file or folder tree. Symlinks are removed, never followed.
fn remove_path(path: &Path, dry_run: bool) -> Step {
    let what = format!("Delete {}", path.display());
    if !path.is_absolute() {
        return Step::new(what, Err("refusing to delete a relative path".to_owned()));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Step::ok(format!("{what} (not present)"));
        }
        Err(error) => return Step::new(what, Err(error.to_string())),
    };
    if dry_run {
        return Step::ok(what);
    }
    let remove = || {
        if metadata.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
    };
    let mut result = remove();
    for _ in 1..REMOVE_ATTEMPTS {
        match &result {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                std::thread::sleep(RETRY_DELAY);
                result = remove();
            }
            _ => break,
        }
    }
    match result {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Step::new(what, Err(error.to_string()))
        }
        _ => Step::ok(what),
    }
}

/// The parent and grandparent of each removed folder, deepest first, but
/// only in the `<ORGANIZATION>/<APPLICATION>/<dir>` shape that `ProjectDirs`
/// uses on Windows (`…\cider-offline\CiderOffline\data`). Other
/// layouts (Linux `~/.local/share/<app>`, macOS, a home folder that happens
/// to be named `cider-offline`) contribute nothing.
fn empty_parent_candidates(removed: &[&Path]) -> Vec<PathBuf> {
    let named = |path: &Path, name: &str| path.file_name().is_some_and(|n| n == name);
    let mut candidates = BTreeSet::new();
    for dir in removed {
        let Some(app) = dir.parent() else { continue };
        let Some(org) = app.parent() else { continue };
        if named(app, AppPaths::APPLICATION) && named(org, AppPaths::ORGANIZATION) {
            candidates.insert(app.to_path_buf());
            candidates.insert(org.to_path_buf());
        }
    }
    let mut candidates: Vec<PathBuf> = candidates.into_iter().collect();
    candidates.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    candidates
}

/// Removes `dir` if it is empty. Returns no step when it is missing or
/// still holds something.
fn remove_empty_dir(dir: &Path, dry_run: bool) -> Option<Step> {
    if !dir.is_dir() {
        return None;
    }
    let what = format!("Delete {} if empty", dir.display());
    if dry_run {
        return Some(Step::ok(what));
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => Some(Step::ok(what)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            None
        }
        Err(error) => Some(Step::new(what, Err(error.to_string()))),
    }
}

/// Text of the removal report: what could not be removed and why.
fn removal_report_text(report: &Report) -> String {
    let mut text = String::from(
        "Cider Offline: \"Remove all data\" could not remove everything.\n\
         These items are still on disk. Delete them by hand, or quit anything \
         that uses them and try again.\n\n",
    );
    for step in report.failures() {
        if let Err(error) = &step.result {
            // Writing to a `String` cannot fail.
            let _ = write!(text, "{}\n    Reason: {error}\n", step.what);
        }
    }
    text
}

/// Writes the removal report into a new file in `dir` and returns its path.
fn write_removal_report(dir: &Path, report: &Report) -> io::Result<PathBuf> {
    write_new_text_file(dir, REMOVAL_REPORT_PREFIX, &removal_report_text(report))
}

/// Writes `text` to a new, uniquely named `<prefix>….txt` file in `dir` and
/// keeps it. Creating it exclusively means an existing file (or a link
/// someone planted in a shared temp folder) is never written through.
fn write_new_text_file(dir: &Path, prefix: &str, text: &str) -> io::Result<PathBuf> {
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".txt")
        .tempfile_in(dir)?;
    file.write_all(text.as_bytes())?;
    let (_, path) = file.keep()?;
    Ok(path)
}

/// The in-app "Remove all data" file step, run by `main` after the proxy
/// stopped and file logging ended: deletes the files, logs the steps and
/// reports failures with [`report_removal_failures`].
pub fn remove_files_after_quit(paths: &AppPaths, ca: CaFolder) {
    tracing::info!("removing all app files");
    let report = remove_files(paths, ca, false);
    report.log();
    report_removal_failures(&report);
}

/// Makes failures of the in-app [`remove_files`] visible: by then the
/// window is gone, file logging has stopped and release builds have no
/// console. Writes a plain-text report to the OS temp folder and opens it
/// in the default text viewer. Does nothing when every step succeeded.
pub fn report_removal_failures(report: &Report) {
    if report.is_ok() {
        return;
    }
    match write_removal_report(&std::env::temp_dir(), report) {
        Ok(path) => {
            if let Err(error) = opener::open(&path) {
                tracing::error!(%error, path = %path.display(), "could not open the removal report");
            }
        }
        Err(error) => tracing::error!(%error, "could not write the removal report"),
    }
}

/// Whether `args` ask for a cleanup run (see [`run_cli`]).
pub fn requested(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == REMOVE_ALL_FLAG || a == UNINSTALL_CLEANUP_FLAG)
}

/// Runs [`REMOVE_ALL_FLAG`] or [`UNINSTALL_CLEANUP_FLAG`] (with
/// [`DRY_RUN_FLAG`]) and prints the steps. Returns `None` when neither flag
/// is given. Refuses while the app is running, and fails when any step failed.
pub fn run_cli(args: &[String], paths: &AppPaths, config: &Config) -> Option<anyhow::Result<()>> {
    if !requested(args) {
        return None;
    }
    let remove_all = args.iter().any(|a| a == REMOVE_ALL_FLAG);
    let dry_run = args.iter().any(|a| a == DRY_RUN_FLAG);
    Some(run(remove_all, dry_run, paths, config))
}

fn run(remove_all: bool, dry_run: bool, paths: &AppPaths, config: &Config) -> anyhow::Result<()> {
    match wait_until_unlocked(&paths.instance_lock_file(), LOCK_WAIT, LOCK_POLL) {
        Ok(true) => {}
        Ok(false) => bail!("Cider Offline is running; quit it first"),
        Err(error) => bail!("could not check whether Cider Offline is running: {error}"),
    }
    let mut report = remove_integration(paths, config, dry_run);
    if remove_all {
        let ca = report.ca_folder();
        report.steps.extend(remove_files(paths, ca, dry_run).steps);
    }
    report.log();
    let text = cli_output(&report, dry_run);
    write!(io::stdout().lock(), "{text}")?;
    // Release builds on Windows have no console, so stdout goes nowhere:
    // leave the plan (and any failure) in a file the user can open.
    if dry_run || !report.is_ok() {
        match write_new_text_file(&std::env::temp_dir(), CLEANUP_LOG_PREFIX, &text) {
            Ok(path) => {
                tracing::info!(path = %path.display(), "cleanup steps written");
                writeln!(io::stdout().lock(), "Steps written to {}", path.display())?;
            }
            Err(error) => tracing::warn!(%error, "could not write the cleanup steps file"),
        }
    }
    let failed = report.failures().count();
    if failed > 0 {
        bail!("{failed} cleanup step(s) failed");
    }
    Ok(())
}

/// What a cleanup flag run prints: a dry-run heading, then every step.
fn cli_output(report: &Report, dry_run: bool) -> String {
    let heading = if dry_run {
        "Dry run: nothing was changed. Planned steps:\n"
    } else {
        ""
    };
    format!("{heading}{report}")
}

/// Waits up to `wait` for the instance lock to be free. `Ok(true)` once it
/// is free; `Ok(false)` if the app still holds it after `wait`.
fn wait_until_unlocked(lock_file: &Path, wait: Duration, poll: Duration) -> io::Result<bool> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if !instance::is_locked(lock_file)? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use cider_offline_core::{AppPaths, Config};

    use super::*;

    fn fill(dir: &Path) {
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("sub").join("b.txt"), b"b").unwrap();
    }

    fn installed(root: &Path) -> AppPaths {
        AppPaths::new(root.join("c"), root.join("d"), root.join("k"))
    }

    #[test]
    fn remove_files_should_delete_installed_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());
        for dir in [paths.config_dir(), paths.data_dir(), paths.cache_dir()] {
            fill(dir);
        }

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(report.is_ok(), "{report}");
        assert!(
            [paths.config_dir(), paths.data_dir(), paths.cache_dir()]
                .iter()
                .all(|dir| !dir.exists())
        );
    }

    /// Creates every app-owned entry of a portable root.
    fn fill_portable(paths: &AppPaths) {
        for entry in paths.owned_entries() {
            let is_file = entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains('.'));
            if is_file {
                fs::write(&entry, b"x").unwrap();
            } else {
                fill(&entry);
            }
        }
    }

    #[test]
    fn remove_files_should_empty_portable_root_but_keep_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let paths = AppPaths::under(&root);
        fill_portable(&paths);

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(report.is_ok(), "{report}");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn remove_files_should_keep_a_foreign_file_in_portable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let paths = AppPaths::under(&root);
        fill_portable(&paths);
        let foreign = root.join("tax-return.pdf");
        fs::write(&foreign, b"mine").unwrap();

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(report.is_ok() && foreign.is_file(), "{report}");
    }

    #[test]
    fn remove_files_should_keep_a_foreign_folder_in_portable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let paths = AppPaths::under(&root);
        fill_portable(&paths);
        let foreign = root.join("Photos");
        fill(&foreign);

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(
            report.is_ok() && foreign.join("sub").join("b.txt").is_file(),
            "{report}"
        );
    }

    #[test]
    fn remove_files_should_not_touch_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());
        fill(paths.data_dir());
        let keep = tmp.path().join("keep.txt");
        fs::write(&keep, b"keep").unwrap();

        let _ = remove_files(&paths, CaFolder::Remove, false);

        assert!(keep.is_file());
    }

    #[test]
    fn remove_files_should_not_touch_siblings_of_portable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        fill(&root);
        let keep = tmp.path().join("keep.txt");
        fs::write(&keep, b"keep").unwrap();

        let _ = remove_files(&AppPaths::under(&root), CaFolder::Remove, false);

        assert!(keep.is_file());
    }

    #[test]
    fn remove_files_dry_run_should_change_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());
        for dir in [paths.config_dir(), paths.data_dir(), paths.cache_dir()] {
            fill(dir);
        }

        let report = remove_files(&paths, CaFolder::Remove, true);

        assert!(report.is_ok(), "{report}");
        assert!(
            [paths.config_dir(), paths.data_dir(), paths.cache_dir()]
                .iter()
                .all(|dir| dir.join("sub").join("b.txt").is_file())
        );
    }

    #[test]
    fn remove_files_dry_run_should_list_every_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());
        for dir in [paths.config_dir(), paths.data_dir(), paths.cache_dir()] {
            fill(dir);
        }

        let report = remove_files(&paths, CaFolder::Remove, true);

        assert_eq!(report.steps.len(), 3, "{report}");
    }

    #[test]
    fn missing_dirs_should_not_be_failures() {
        let tmp = tempfile::tempdir().unwrap();

        let report = remove_files(&installed(tmp.path()), CaFolder::Remove, false);

        assert!(report.is_ok(), "{report}");
    }

    #[test]
    fn missing_portable_root_should_not_be_a_failure() {
        let tmp = tempfile::tempdir().unwrap();

        let report = remove_files(
            &AppPaths::under(&tmp.path().join("data")),
            CaFolder::Remove,
            false,
        );

        assert!(report.is_ok(), "{report}");
    }

    #[test]
    fn remove_files_should_delete_empty_parents_up_to_app_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join(AppPaths::ORGANIZATION);
        let inner = app.join(AppPaths::APPLICATION);
        let paths = AppPaths::new(
            inner.join("config"),
            inner.join("data"),
            inner.join("cache"),
        );
        fill(paths.data_dir());
        fill(paths.cache_dir());

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(report.is_ok(), "{report}");
        assert!(!app.exists() && tmp.path().is_dir());
    }

    #[test]
    fn remove_files_should_keep_parents_holding_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join(AppPaths::ORGANIZATION);
        let inner = app.join(AppPaths::APPLICATION);
        let paths = AppPaths::new(
            inner.join("config"),
            inner.join("data"),
            inner.join("cache"),
        );
        fill(paths.data_dir());
        let foreign = app.join("other.txt");
        fs::write(&foreign, b"x").unwrap();

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(
            report.is_ok() && foreign.is_file() && !inner.exists(),
            "{report}"
        );
    }

    #[test]
    fn remove_files_should_delete_a_log_file_that_is_still_open() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());
        let mut appender = tracing_appender::rolling::never(paths.logs_dir(), "app.log");
        std::io::Write::write_all(&mut appender, b"line\n").unwrap();

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(report.is_ok() && !paths.cache_dir().exists(), "{report}");
        drop(appender);
    }

    #[test]
    fn run_cli_should_ignore_unrelated_args() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());

        assert!(run_cli(&["x".into()], &paths, &Config::default()).is_none());
    }

    #[test]
    fn run_cli_should_refuse_while_the_app_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());
        fill(paths.data_dir());
        let lock = fs::File::create(paths.instance_lock_file()).unwrap();
        lock.lock().unwrap();
        let args = [REMOVE_ALL_FLAG.to_owned(), DRY_RUN_FLAG.to_owned()];

        let result = run_cli(&args, &paths, &Config::default());

        assert!(matches!(result, Some(Err(_))));
    }

    #[test]
    fn report_should_fail_when_any_step_fails() {
        let report = Report {
            steps: vec![Step::ok("a"), Step::new("b", Err("denied".into()))],
        };

        assert!(!report.is_ok());
    }

    #[test]
    fn should_revert_cider_only_when_it_uses_this_proxy() {
        let state = |use_proxy, points_at_us| cider::CiderProxyState {
            use_proxy,
            proxy_server: "127.0.0.1:8000".into(),
            points_at_us,
        };

        assert_eq!(
            [
                should_revert(&state(true, true)),
                should_revert(&state(true, false)),
                should_revert(&state(false, true)),
            ],
            [true, false, false]
        );
    }

    /// Linux `ProjectDirs` has no organisation folder: `~/.local/share/<app>`.
    /// A home folder that happens to be named `cider-offline` must not
    /// start a parent walk that deletes `.local/share`, `.config` or `.cache`.
    #[test]
    fn remove_files_should_not_walk_up_a_home_named_like_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(AppPaths::ORGANIZATION);
        let paths = AppPaths::new(
            home.join(".config").join("cideroffline"),
            home.join(".local").join("share").join("cideroffline"),
            home.join(".cache").join("cideroffline"),
        );
        for dir in [paths.config_dir(), paths.data_dir(), paths.cache_dir()] {
            fill(dir);
        }

        let report = remove_files(&paths, CaFolder::Remove, false);

        assert!(
            report.is_ok()
                && [".config", ".local/share", ".cache"]
                    .iter()
                    .all(|dir| home.join(dir).is_dir()),
            "{report}"
        );
    }

    #[test]
    fn remove_files_should_keep_ca_folder_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join(AppPaths::ORGANIZATION);
        let inner = app.join(AppPaths::APPLICATION);
        let paths = AppPaths::new(
            inner.join("config"),
            inner.join("data"),
            inner.join("cache"),
        );
        fill(paths.data_dir());
        fill(&paths.ca_dir());

        let report = remove_files(&paths, CaFolder::Keep, false);

        let left: Vec<_> = fs::read_dir(paths.data_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            report.is_ok() && left == ["ca"] && paths.ca_dir().join("a.txt").is_file(),
            "{report}"
        );
    }

    #[test]
    fn remove_files_should_keep_ca_folder_in_portable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let paths = AppPaths::under(&root);
        fill(&root.join("media"));
        fill(&paths.ca_dir());

        let report = remove_files(&paths, CaFolder::Keep, false);

        let left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(report.is_ok() && left == ["ca"], "{report}");
    }

    #[test]
    fn ca_folder_should_be_kept_only_when_certificate_step_failed() {
        let report = |kind| Report {
            steps: vec![Step::new("x", Err("denied".into())).of(kind)],
        };

        assert_eq!(
            [
                report(StepKind::Certificate).ca_folder(),
                report(StepKind::Autostart).ca_folder(),
                Report::default().ca_folder(),
            ],
            [CaFolder::Keep, CaFolder::Remove, CaFolder::Remove]
        );
    }

    #[test]
    fn leftovers_should_list_only_failed_integration_steps() {
        let report = Report {
            steps: vec![
                Step::ok("cert").of(StepKind::Certificate),
                Step::new("login", Err("denied".into())).of(StepKind::Autostart),
                Step::new("file", Err("locked".into())),
            ],
        };

        assert_eq!(
            report.leftovers(),
            [StepKind::Autostart.leftover().unwrap()]
        );
    }

    fn failed_removal() -> Report {
        Report {
            steps: vec![
                Step::ok("Delete /app/config"),
                Step::new("Delete /app/media", Err("file in use".into())),
            ],
        }
    }

    #[test]
    fn removal_report_text_should_list_failures_with_reasons() {
        let text = removal_report_text(&failed_removal());

        assert_eq!(
            text,
            "Cider Offline: \"Remove all data\" could not remove everything.\n\
             These items are still on disk. Delete them by hand, or quit anything that \
             uses them and try again.\n\n\
             Delete /app/media\n    Reason: file in use\n"
        );
    }

    #[test]
    fn write_removal_report_should_write_the_report_file() {
        let tmp = tempfile::tempdir().unwrap();

        let path = write_removal_report(tmp.path(), &failed_removal()).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            removal_report_text(&failed_removal())
        );
    }

    #[test]
    fn write_removal_report_should_never_reuse_a_file() {
        let tmp = tempfile::tempdir().unwrap();

        let first = write_removal_report(tmp.path(), &failed_removal()).unwrap();
        let second = write_removal_report(tmp.path(), &failed_removal()).unwrap();

        assert!(
            first != second
                && [&first, &second].iter().all(|path| {
                    path.parent() == Some(tmp.path())
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with(REMOVAL_REPORT_PREFIX))
                }),
            "{first:?} {second:?}"
        );
    }

    #[test]
    fn cli_output_should_head_a_dry_run() {
        let report = Report {
            steps: vec![Step::ok("Delete /app/media")],
        };

        assert_eq!(
            cli_output(&report, true),
            "Dry run: nothing was changed. Planned steps:\nok      Delete /app/media\n"
        );
    }

    #[test]
    fn wait_until_unlocked_should_see_a_lock_released_while_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("instance.lock");
        let lock = fs::File::create(&lock_file).unwrap();
        lock.lock().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(lock);
        });

        let free = wait_until_unlocked(
            &lock_file,
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .unwrap();

        releaser.join().unwrap();
        assert!(free);
    }

    #[test]
    fn wait_until_unlocked_should_give_up_after_the_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("instance.lock");
        let lock = fs::File::create(&lock_file).unwrap();
        lock.lock().unwrap();

        let free = wait_until_unlocked(
            &lock_file,
            Duration::from_millis(100),
            Duration::from_millis(20),
        )
        .unwrap();

        assert!(!free);
        drop(lock);
    }

    #[test]
    fn autostart_step_should_fail_when_the_entry_cannot_be_read() {
        let step = autostart_step(Err("access denied".into()), false, || Ok(()));

        assert!(step.result.is_err());
    }

    #[test]
    fn autostart_step_should_skip_a_disabled_entry() {
        let step = autostart_step(Ok(false), false, || Err("must not run".into()));

        assert!(step.result.is_ok());
    }

    #[test]
    fn removal_locations_should_name_the_portable_root() {
        let root = Path::new("/p/data");

        let text = removal_locations(&AppPaths::under(root));

        assert!(text.contains(&root.display().to_string()), "{text}");
    }

    #[test]
    fn removal_locations_should_list_each_installed_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = installed(tmp.path());

        let text = removal_locations(&paths);

        assert_eq!(text.lines().count(), 3, "{text}");
    }
}
