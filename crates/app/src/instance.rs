//! Single-instance guard: an OS file lock plus a local activation socket.
//!
//! The lock file (`instance.lock` in the data folder) decides who is primary:
//! whoever holds the exclusive lock owns the data folder. The primary also
//! listens on a local socket named after the user and the data folder (a
//! named pipe on Windows, an abstract socket on Linux, a socket file in the
//! per-user temp dir on macOS), so copies with different data folders never
//! share one. A later
//! launch that cannot take the lock connects to that socket and asks the
//! primary to show its window, then exits.
//!
//! Handshake (one line per message):
//! 1. secondary → `hello`
//! 2. primary → `<pid>`
//! 3. secondary grants the primary foreground permission
//!    ([`crate::platform::allow_foreground`]), then sends `show` or
//!    `show <activation token>`
//! 4. the primary calls the `on_show` callback with the token.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::ConnectWaitMode;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{
    ConnectOptions, GenericFilePath, GenericNamespaced, Listener, ListenerOptions, Name, Stream,
};

const SOCKET_PREFIX: &str = "cider-offline";
/// Per-attempt connect timeout of the secondary.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the secondary keeps retrying the connection.
const CONNECT_RETRY_FOR: Duration = Duration::from_secs(2);
/// Pause between connection attempts.
const CONNECT_RETRY_PAUSE: Duration = Duration::from_millis(100);
/// Receive timeout on both ends of the handshake.
const RECV_TIMEOUT: Duration = Duration::from_secs(1);
/// Connect timeout when probing whether a socket file has a live listener.
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);
/// Longest request line the primary reads; real ones are far shorter.
const MAX_REQUEST_LINE: u64 = 256;
/// Most activation clients handled at once; later ones are turned away.
const MAX_CLIENTS: usize = 8;

/// Result of [`acquire`].
pub enum Instance {
    /// This process holds the lock; start the app and call [`Primary::serve`].
    Primary(Primary),
    /// Another instance holds the lock and was asked to show its window.
    Secondary,
}

/// The primary instance's guard. Dropping it releases the OS lock.
pub struct Primary {
    #[expect(dead_code, reason = "held only so its Drop releases the OS lock")]
    lock: File,
    /// `None` when the activation socket could not be created (its name is
    /// held by another process).
    listener: Option<Listener>,
}

/// The primary's `show` callback, shared by the per-client threads.
type OnShow = Mutex<dyn Fn(Option<String>) + Send>;

/// A request from a secondary instance.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Hello,
    Show { token: Option<String> },
}

/// Becomes the primary instance, or signals the running one.
///
/// The socket name is derived from the lock file's folder, so copies with
/// different data folders (portable next to installed) never share one.
pub fn acquire(lock_file: &Path) -> io::Result<Instance> {
    let data_dir = lock_file.parent().unwrap_or(lock_file);
    std::fs::create_dir_all(data_dir)?;
    acquire_named(lock_file, socket_name(data_dir)?)
}

/// Whether another process holds the lock on `lock_file`.
///
/// Takes and immediately releases the lock; never contacts the running
/// instance.
pub fn is_locked(lock_file: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).open(lock_file) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// [`acquire`] with an explicit socket name (tests use unique names).
pub(crate) fn acquire_named(lock_file: &Path, name: Name<'static>) -> io::Result<Instance> {
    if let Some(parent) = lock_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_file)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            match signal(&name, activation_token_from_env().as_deref()) {
                Ok(pid) => tracing::info!(pid, "asked the running instance to show its window"),
                Err(error) => tracing::warn!(%error, "could not signal the running instance"),
            }
            return Ok(Instance::Secondary);
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }

    let listener = match create_listener(name) {
        Ok(listener) => Some(listener),
        Err(error) if is_name_taken(&error) => {
            tracing::warn!(
                %error,
                "activation socket name is held by another process; starting without it"
            );
            None
        }
        Err(error) => {
            tracing::warn!(%error, "could not create the activation socket; starting without it");
            None
        }
    };
    Ok(Instance::Primary(Primary { lock, listener }))
}

impl Primary {
    /// Answers activation requests on a background thread, calling `on_show`
    /// with the secondary's activation token for each `show`.
    ///
    /// The lock stays with `self`; only the listener moves to the thread.
    pub fn serve(&mut self, on_show: impl Fn(Option<String>) + Send + 'static) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        // Clients are handled on their own threads; the mutex makes the
        // callback shareable without requiring `Sync` from the caller.
        let on_show: Arc<OnShow> = Arc::new(Mutex::new(on_show));
        let clients = Arc::new(AtomicUsize::new(0));
        let spawned = thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || {
                for connection in listener.incoming() {
                    match connection {
                        Ok(stream) => {
                            // Dropping the stream turns the client away.
                            let Some(slot) = ClientSlot::take(&clients) else {
                                tracing::debug!("too many activation clients; refusing one");
                                continue;
                            };
                            let on_show = Arc::clone(&on_show);
                            // One thread per client: Windows pipes have no
                            // timeouts, so a stuck client must not block others.
                            let handled = thread::Builder::new()
                                .name("single-instance-client".into())
                                .spawn(move || {
                                    let _slot = slot;
                                    if let Err(error) = handle_client(stream, &on_show) {
                                        tracing::debug!(%error, "activation request failed");
                                    }
                                });
                            if let Err(error) = handled {
                                tracing::warn!(%error, "could not handle an activation request");
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "activation connection failed");
                        }
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the single-instance listener");
        }
    }
}

/// One of the [`MAX_CLIENTS`] client-thread slots; dropping it frees the slot.
struct ClientSlot(Arc<AtomicUsize>);

impl ClientSlot {
    /// Takes a free slot, or `None` when [`MAX_CLIENTS`] are in use.
    fn take(clients: &Arc<AtomicUsize>) -> Option<Self> {
        clients
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CLIENTS).then_some(n + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(clients)))
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Runs the primary's side of the handshake for one client.
///
/// Each line is read up to [`MAX_REQUEST_LINE`] bytes. On Windows a client
/// that connects and never writes keeps this thread blocked (pipes have no
/// receive timeout); [`MAX_CLIENTS`] bounds how many such threads exist.
fn handle_client(stream: Stream, on_show: &OnShow) -> io::Result<()> {
    set_timeouts(&stream)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if (&mut reader).take(MAX_REQUEST_LINE).read_line(&mut line)? == 0 {
            return Ok(());
        }
        match parse_request(&line) {
            Some(Request::Hello) => {
                writeln!(reader.get_mut(), "{}", std::process::id())?;
                reader.get_mut().flush()?;
            }
            Some(Request::Show { token }) => {
                let on_show = on_show.lock().unwrap_or_else(PoisonError::into_inner);
                on_show(token);
                return Ok(());
            }
            None => {
                tracing::debug!("ignoring an unknown activation request");
                return Ok(());
            }
        }
    }
}

/// Runs the secondary's side of the handshake. Returns the primary's pid.
fn signal(name: &Name<'_>, token: Option<&str>) -> io::Result<u32> {
    let mut stream = connect_with_retry(name)?;
    set_timeouts(&stream)?;
    stream.write_all(b"hello\n")?;
    stream.flush()?;

    // Read the pid on a helper thread: Windows pipes ignore receive
    // timeouts, and `allow_foreground` must run on this (main) thread.
    let (reply_tx, reply_rx) = mpsc::channel();
    thread::Builder::new()
        .name("single-instance-signal".into())
        .spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let reply = reader.read_line(&mut line).and_then(|_| parse_pid(&line));
            let _ = reply_tx.send(reply.map(|pid| (reader.into_inner(), pid)));
        })?;
    let (mut stream, pid) = reply_rx.recv_timeout(RECV_TIMEOUT).map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "no reply from the running instance",
        )
    })??;

    crate::platform::allow_foreground(pid);
    stream.write_all(show_line(token).as_bytes())?;
    stream.flush()?;
    Ok(pid)
}

/// Connects with a per-attempt timeout, retrying until [`CONNECT_RETRY_FOR`]
/// has passed (the primary may still be starting).
fn connect_with_retry(name: &Name<'_>) -> io::Result<Stream> {
    let deadline = Instant::now() + CONNECT_RETRY_FOR;
    loop {
        match connect(name, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(CONNECT_RETRY_PAUSE),
        }
    }
}

fn connect(name: &Name<'_>, timeout: Duration) -> io::Result<Stream> {
    ConnectOptions::new()
        .name(name.borrow())
        .wait_mode(ConnectWaitMode::Timeout(timeout))
        .connect_sync()
}

/// Sets the receive timeout; named pipes do not support one (the callers
/// bound the wait differently there).
fn set_timeouts(stream: &Stream) -> io::Result<()> {
    match stream.set_recv_timeout(Some(RECV_TIMEOUT)) {
        Err(error) if error.kind() != io::ErrorKind::Unsupported => Err(error),
        _ => Ok(()),
    }
}

/// Creates the listener. Socket files (macOS) can outlive a crash, so a file
/// nobody answers on is overwritten, but a live listener is never displaced.
fn create_listener(name: Name<'static>) -> io::Result<Listener> {
    let file_based = name.is_path();
    match ListenerOptions::new().name(name.borrow()).create_sync() {
        Err(error)
            if file_based
                && error.kind() == io::ErrorKind::AddrInUse
                && connect(&name, PROBE_TIMEOUT).is_err() =>
        {
            ListenerOptions::new()
                .name(name)
                .try_overwrite(true)
                .create_sync()
        }
        result => result,
    }
}

/// Whether a listener error means another process owns the socket name.
fn is_name_taken(error: &io::Error) -> bool {
    match error.kind() {
        io::ErrorKind::AddrInUse => true,
        // A second first-instance named pipe fails with ERROR_ACCESS_DENIED.
        io::ErrorKind::PermissionDenied => cfg!(windows),
        _ => false,
    }
}

fn parse_request(line: &str) -> Option<Request> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line == "hello" {
        return Some(Request::Hello);
    }
    if line == "show" {
        return Some(Request::Show { token: None });
    }
    let token = line.strip_prefix("show ")?.trim();
    Some(Request::Show {
        token: (!token.is_empty()).then(|| token.to_owned()),
    })
}

fn parse_pid(line: &str) -> io::Result<u32> {
    line.trim()
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pid reply"))
}

fn show_line(token: Option<&str>) -> String {
    match token {
        Some(token) => format!("show {token}\n"),
        None => "show\n".to_owned(),
    }
}

/// The desktop activation token this process was started with, so the
/// primary can raise its window under Wayland/X11 focus-stealing prevention.
fn activation_token_from_env() -> Option<String> {
    if cfg!(target_os = "linux") {
        token_from(|key| std::env::var(key).ok())
    } else {
        None
    }
}

/// `XDG_ACTIVATION_TOKEN`, else `DESKTOP_STARTUP_ID`. Values that would break
/// the line protocol (empty or containing whitespace) are ignored.
fn token_from(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .into_iter()
        .filter_map(var)
        .find(|token| !token.is_empty() && !token.contains(char::is_whitespace))
}

/// Socket name for the copy of the app using `data_dir`, for this user:
/// named pipes and abstract sockets are machine-wide.
fn socket_name(data_dir: &Path) -> io::Result<Name<'static>> {
    let data_dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    name_for(&socket_file_name(&user_id(), &data_dir))
}

/// `cider-offline-<hash>.sock`, where the hash covers the user and the
/// data folder. Stable across runs and builds ([`fnv1a`]), so a later
/// launch always finds the running copy that owns the same folder.
fn socket_file_name(user: &str, data_dir: &Path) -> String {
    let mut bytes = user.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(data_dir.as_os_str().as_encoded_bytes());
    format!("{SOCKET_PREFIX}-{:016x}.sock", fnv1a(&bytes))
}

/// 64-bit FNV-1a. Unlike `DefaultHasher`, its output is fixed, so two
/// different builds of the app agree on the socket name.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// A namespaced name where supported, else a socket file in the temp dir
/// (macOS: the per-user temp dir already separates users).
fn name_for(file: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        file.to_ns_name::<GenericNamespaced>().map(Name::into_owned)
    } else {
        std::env::temp_dir()
            .join(file)
            .to_fs_name::<GenericFilePath>()
            .map(Name::into_owned)
    }
}

/// Who runs this process, for [`socket_file_name`]: the uid on Unix.
#[cfg(unix)]
fn user_id() -> String {
    format!("uid:{}", rustix::process::getuid().as_raw())
}

/// Who runs this process, for [`socket_file_name`]: the domain and login
/// name on Windows (hashed, so any characters are fine).
#[cfg(not(unix))]
fn user_id() -> String {
    let var = |key| std::env::var(key).unwrap_or_default();
    format!("user:{}\\{}", var("USERDOMAIN"), var("USERNAME"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A socket name no other test (or running app) uses.
    fn unique_name() -> Name<'static> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        name_for(&format!("cider-offline-test-{}-{n}", std::process::id())).unwrap()
    }

    fn lock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("instance.lock")
    }

    fn primary(instance: Instance) -> Primary {
        match instance {
            Instance::Primary(primary) => primary,
            Instance::Secondary => panic!("expected the primary instance"),
        }
    }

    #[test]
    fn parse_request_should_read_hello_and_show() {
        assert!(matches!(parse_request("hello"), Some(Request::Hello)));
        assert!(matches!(
            parse_request("show"),
            Some(Request::Show { token: None })
        ));
        assert!(
            matches!(parse_request("show tok_1"), Some(Request::Show { token: Some(t) }) if t == "tok_1")
        );
        assert!(parse_request("junk").is_none());
    }

    #[test]
    fn parse_request_should_strip_line_ending() {
        assert_eq!(parse_request("hello\r\n"), Some(Request::Hello));
    }

    #[test]
    fn socket_file_name_should_be_stable_across_runs() {
        // A fixed value: changing the derivation breaks activation between
        // a running copy and a launch of another build.
        assert_eq!(
            socket_file_name("uid:1000", Path::new("/data")),
            "cider-offline-e5785df619028359.sock"
        );
    }

    #[test]
    fn fnv1a_should_match_the_reference_vector() {
        // FNV-1a 64 of "a", from the FNV reference test suite.
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn socket_file_name_should_differ_per_data_folder() {
        assert_ne!(
            socket_file_name("u", Path::new("/opt/app/data")),
            socket_file_name("u", Path::new("/home/u/.local/share/cideroffline"))
        );
    }

    #[test]
    fn socket_file_name_should_differ_per_user() {
        let dir = Path::new("/shared/data");
        assert_ne!(
            socket_file_name("uid:1000", dir),
            socket_file_name("uid:1001", dir)
        );
    }

    #[test]
    fn socket_file_name_should_be_safe_for_any_user_name() {
        let name = socket_file_name("user:DOMAIN\\Zo\u{eb} O'Brien", Path::new(r"C:\data"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.')),
            "{name}"
        );
    }

    #[test]
    fn client_slots_should_stop_at_the_cap() {
        let clients = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_CLIENTS)
            .map(|_| ClientSlot::take(&clients).unwrap())
            .collect();

        assert!(ClientSlot::take(&clients).is_none());
        drop(slots);
    }

    #[test]
    fn client_slots_should_free_on_drop() {
        let clients = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_CLIENTS)
            .map(|_| ClientSlot::take(&clients).unwrap())
            .collect();
        drop(slots);

        assert!(ClientSlot::take(&clients).is_some());
    }

    #[test]
    fn handle_client_should_cap_the_request_line() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name();
        let mut primary = primary(acquire_named(&lock_path(&dir), name.clone()).unwrap());
        let (tx, rx) = mpsc::channel();
        primary.serve(move |token| {
            let _ = tx.send(token);
        });
        let mut stream = connect_with_retry(&name).unwrap();
        let long = format!("show {}\n", "x".repeat(4096));

        let _ = stream.write_all(long.as_bytes());
        let token = rx.recv_timeout(Duration::from_secs(2)).unwrap();

        assert!(token.is_some_and(|t| t.len() < usize::try_from(MAX_REQUEST_LINE).unwrap()));
    }

    #[test]
    fn show_line_should_append_token() {
        assert_eq!(show_line(Some("tok")), "show tok\n");
    }

    #[test]
    fn show_line_should_be_bare_without_token() {
        assert_eq!(show_line(None), "show\n");
    }

    #[test]
    fn token_from_should_prefer_xdg_activation_token() {
        let token = token_from(|key| Some(format!("{key}-value")));
        assert_eq!(token.as_deref(), Some("XDG_ACTIVATION_TOKEN-value"));
    }

    #[test]
    fn token_from_should_fall_back_to_desktop_startup_id() {
        let token = token_from(|key| (key == "DESKTOP_STARTUP_ID").then(|| "id_1".to_owned()));
        assert_eq!(token.as_deref(), Some("id_1"));
    }

    #[test]
    fn token_from_should_skip_values_that_break_the_protocol() {
        let token = token_from(|key| match key {
            "XDG_ACTIVATION_TOKEN" => Some("a b\n".to_owned()),
            _ => Some(String::new()),
        });
        assert_eq!(token, None);
    }

    #[test]
    fn second_acquire_should_be_secondary_while_first_holds_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _first = primary(acquire_named(&lock_path(&dir), unique_name()).unwrap());
        let second = acquire_named(&lock_path(&dir), unique_name()).unwrap();
        assert!(matches!(second, Instance::Secondary));
    }

    #[test]
    fn dropping_primary_should_allow_new_primary() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name();
        drop(primary(
            acquire_named(&lock_path(&dir), name.clone()).unwrap(),
        ));
        let again = acquire_named(&lock_path(&dir), name).unwrap();
        assert!(matches!(
            again,
            Instance::Primary(Primary {
                listener: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn handshake_should_deliver_show_with_token() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name();
        let mut primary = primary(acquire_named(&lock_path(&dir), name.clone()).unwrap());
        let (tx, rx) = mpsc::channel();
        primary.serve(move |token| {
            let _ = tx.send(token);
        });

        assert_eq!(signal(&name, Some("tok")).unwrap(), std::process::id());
        let token = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(token.as_deref(), Some("tok"));
    }

    /// `main` starts serving right after `acquire` but drains the channel only
    /// once the UI is up; a `show` sent in between must still arrive.
    #[test]
    fn show_sent_before_consumer_attaches_should_still_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name();
        let mut primary = primary(acquire_named(&lock_path(&dir), name.clone()).unwrap());
        let (tx, rx) = mpsc::channel();
        primary.serve(move |token| {
            let _ = tx.send(token);
        });

        // Both handshakes complete while nobody reads `rx`.
        signal(&name, Some("early-1")).unwrap();
        signal(&name, None).unwrap();

        let mut tokens: Vec<Option<String>> = (0..2)
            .map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap())
            .collect();
        tokens.sort();
        assert_eq!(tokens, [None, Some("early-1".to_owned())]);
    }

    #[test]
    fn primary_should_start_without_listener_when_name_taken() {
        let name = unique_name();
        let _other = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let instance = acquire_named(&lock_path(&dir), name).unwrap();
        assert!(matches!(
            instance,
            Instance::Primary(Primary { listener: None, .. })
        ));
    }

    #[test]
    fn is_locked_should_be_true_while_primary_holds_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _primary = primary(acquire_named(&lock_path(&dir), unique_name()).unwrap());
        assert!(is_locked(&lock_path(&dir)).unwrap());
    }

    #[test]
    fn is_locked_should_be_false_after_primary_drops() {
        let dir = tempfile::tempdir().unwrap();
        drop(primary(
            acquire_named(&lock_path(&dir), unique_name()).unwrap(),
        ));
        assert!(!is_locked(&lock_path(&dir)).unwrap());
    }

    #[test]
    fn is_locked_should_be_false_without_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_locked(&lock_path(&dir)).unwrap());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn stale_socket_file_should_not_block_new_primary() {
        let file = std::env::temp_dir().join(format!(
            "cider-offline-test-stale-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&file, b"").unwrap();
        let name = file.to_fs_name::<GenericFilePath>().unwrap().into_owned();
        let dir = tempfile::tempdir().unwrap();
        let instance = acquire_named(&lock_path(&dir), name).unwrap();
        assert!(matches!(
            instance,
            Instance::Primary(Primary {
                listener: Some(_),
                ..
            })
        ));
    }
}
