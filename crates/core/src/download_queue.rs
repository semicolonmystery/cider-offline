//! Background download queue driving the registered downloader for a batch of
//! pre-resolved tracks.
//!
//! The queue is a small actor: callers push [`Command`]s through an mpsc
//! channel, workers run in parallel bounded by an adjustable concurrency
//! limit, and progress is published on tokio broadcast channels. The database
//! is the source of truth for every row so the queue survives restarts.
//!
//! # Being gentle with Apple
//!
//! Evidence from real runs: license requests fail with an empty body almost
//! only when many of them are in flight at once (≥ 7 within a second: 16 of 23
//! failed; 2–3 within a second: 0 of 61). So the queue
//!
//! - runs few downloads at once (default [`DEFAULT_CONCURRENCY`], user
//!   adjustable 1–[`MAX_CONCURRENCY`] via [`DownloadQueue::set_concurrency`]),
//! - spaces fetch starts at least [`START_SPACING`] apart (each fetch begins
//!   with the playback + license requests), and backs every start off by
//!   [`FAILURE_COOLDOWN`] after a temporary failure,
//! - retries temporary failures with exponential backoff plus jitter, at most
//!   [`MAX_ATTEMPTS`] attempts in total, and
//! - never retries failures it cannot fix by retrying: those rows become
//!   [`DownloadStatus::Unavailable`] (shown as "Can't download yet").
//!
//! # Single source of truth
//!
//! The queue owns every download's state and the UI only renders what it
//! publishes. A row moves `Queued -> Downloading (Requesting -> License ->
//! Fetching -> Decrypting -> Saving) -> Done`, or ends `Failed` /
//! `Unavailable`, is held `Paused`, or is removed (cancelled). The
//! [`DownloadStage`] comes from the downloader's own reports whenever it sends
//! any, otherwise from an estimate based on earlier download times.
//!
//! # Contract with the downloader
//!
//! Before each fetch the queue resolves what to download (the catalog id, the
//! exact request shape Cider uses, the media kind, and the playable twin of a
//! library-only item), skips music videos unless the downloader declares
//! [`DownloaderCapabilities::music_videos`], and maps the typed
//! [`DownloadError`] to retry / no retry / re-sign-in. The downloader never
//! retries or sleeps itself. The full contract is on
//! [`crate::download::Downloader`].
//!
//! # Download format
//!
//! [`DownloadQueue::set_download_format`] picks which versions a download
//! fetches ([`DownloadFormat`]). For anything but `Normal` the queue asks the
//! catalog for the song with `extend=extendedAssetUrls` (using the captured
//! developer token, in the storefront the proxy saw) and reads
//! `attributes.extendedAssetUrls.enhancedHls`: the spatial stream is the
//! binaural variant of that master playlist. A fresh lookup is used instead
//! of the response cache because the cache only holds the exact URLs Cider
//! happened to request, and songs queued from a collection were usually
//! never requested that way. Songs whose `audioTraits` lack `atmos` have no
//! spatial version and skip the master playlist. Each version is its own
//! fetch (with its own license requests), so every fetch start, including
//! the second version of a song, goes through the same start spacing.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::DownloadFormat;
use crate::catalog::CatalogClient;
use crate::catalog::{CatalogItem, Song};
use crate::download::{
    DownloadError, DownloadProgress, DownloadRequest, DownloadStage, Downloaded,
    DownloaderCapabilities, DownloaderRegistry, MediaKind, ProgressReport, TrackInfo,
};
use crate::download_cleanup::UnlinkedReport;
use crate::download_target::{self, MUSIC_VIDEO_REASON, SpatialLookup, Target, TwinSearch};
use crate::library::{Library, LibraryError};
use crate::media::{AUDIO_EXTENSIONS, CUSTOM_SONG_PREFIX, MediaResolver};
use crate::storage::{
    AudioFile, AudioVariant, CustomAudioMode, DownloadRecord, DownloadSourceKind, DownloadStatus,
    LyricsSource, Storage, StorageError,
};

/// Default number of simultaneous downloads.
pub const DEFAULT_CONCURRENCY: usize = 2;
/// Largest accepted number of simultaneous downloads.
pub const MAX_CONCURRENCY: usize = 6;
/// Total attempts (first try + retries) for a temporary failure.
pub const MAX_ATTEMPTS: u32 = 4;
/// Minimum spacing between the starts of two fetches.
pub const START_SPACING: Duration = Duration::from_millis(1_500);
/// Extra pause before any new fetch starts after a temporary failure.
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(8);
/// First retry delay; each later retry waits [`BACKOFF_FACTOR`] times longer.
const BACKOFF_BASE: Duration = Duration::from_secs(4);
/// Growth of the retry delay per attempt.
const BACKOFF_FACTOR: u32 = 3;
/// Upper bound of one retry delay.
const BACKOFF_MAX: Duration = Duration::from_secs(90);
/// Upper bound honoured for Apple's `Retry-After`.
const RETRY_AFTER_MAX: Duration = Duration::from_mins(10);
/// Row message of a download stopped because Apple rejected the tokens.
const AUTH_EXPIRED_HINT: &str =
    "Apple Music sign-in expired: play a song in Cider while online, then retry";
/// Capacity of the public [`DownloadEvent`] broadcast channel.
const EVENTS_CAPACITY: usize = 1024;
/// Capacity of the public [`DownloadProgressUpdate`] broadcast channel.
const PROGRESS_CAPACITY: usize = 1024;
/// Smallest progress-fraction change published while a download runs.
const PROGRESS_STEP: f32 = 0.01;
/// Smallest progress-fraction change persisted to SQLite (stage changes are
/// always persisted).
const PERSIST_STEP: f32 = 0.1;
/// How often a running fetch re-evaluates its (estimated) progress.
const PROGRESS_TICK: Duration = Duration::from_millis(250);
/// Initial estimate of one fetch's duration, refined from finished downloads.
const INITIAL_FETCH_ESTIMATE_MS: u64 = 6_000;
/// Downloader id looked up for every queued track.
const DOWNLOADER_ID: &str = "drm";
/// Seconds of playback silence required before deferred auto-cache rows start.
pub const AUTO_CACHE_IDLE_SECS: u64 = 30;
/// How often the actor wakes up to re-check the deferred backlog.
const DEFERRED_POLL: Duration = Duration::from_secs(5);
/// How long a paused or cancelled fetch may take to stop (the downloader
/// checks its cancel token between chunks and before decrypting and saving)
/// before the queue drops it anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(30);
/// Temporary files a crashed download left in the media folder are deleted
/// at startup once they are at least this old.
const STALE_TEMP_AGE: Duration = Duration::from_mins(10);

/// Fetches lyrics for a track once its audio download has completed.
///
/// The queue calls this after every successful download (manual song,
/// album / playlist, and auto-cache rows alike). It runs off the download
/// slot, and its outcome never changes the download row: a failure is only
/// logged.
#[async_trait]
pub trait LyricsFetcher: Send + Sync {
    /// Fetches and stores lyrics for `track_id` (the queued catalog id),
    /// skipping tracks that already have lyrics. Returns `None` when skipped.
    ///
    /// # Errors
    ///
    /// Lookup, file or database errors.
    async fn fetch_lyrics(
        &self,
        track_id: &str,
        info: &TrackInfo,
    ) -> Result<Option<LyricsSource>, LibraryError>;

    /// Called whenever the queue's state changes: `idle` is `true` when no
    /// download is queued or running. Lyrics lookups deferred after a
    /// failure are retried once the queue is idle.
    fn downloads_idle(&self, idle: bool) {
        let _ = idle;
    }
}

#[async_trait]
impl LyricsFetcher for Library {
    async fn fetch_lyrics(
        &self,
        track_id: &str,
        info: &TrackInfo,
    ) -> Result<Option<LyricsSource>, LibraryError> {
        self.ensure_download_lyrics(track_id, info).await
    }

    fn downloads_idle(&self, idle: bool) {
        self.set_downloads_idle(idle);
    }
}

/// Errors that can surface from a [`DownloadQueue`] public method.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// The queue actor has stopped and no longer accepts commands.
    #[error("download queue is no longer running")]
    Closed,
}

/// Progress event emitted by a running [`DownloadQueue`].
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    /// A new row was inserted.
    Added(DownloadRecord),
    /// A row's status / progress / message changed.
    StatusChanged {
        /// Row id.
        id: String,
        /// New status.
        status: DownloadStatus,
        /// Fractional progress in `0.0..=1.0`.
        progress: f32,
        /// Error, "can't download yet" reason, or retry note ("Retrying in 12 s").
        message: Option<String>,
        /// Current phase of a running download (`None` when not running).
        stage: Option<DownloadStage>,
        /// 1-based attempt number (greater than 1 while retrying).
        attempt: u32,
    },
    /// A row was removed from the queue.
    Removed(String),
    /// The local audio of these download keys (catalog ids) was deleted.
    AudioRemoved(Vec<String>),
    /// Aggregate counters by status.
    SummaryChanged {
        /// Rows currently being downloaded (or paused).
        active: usize,
        /// Rows waiting for a worker.
        queued: usize,
        /// Successfully finished rows.
        done: usize,
        /// Rows that failed.
        failed: usize,
        /// Rows that can never be downloaded.
        unavailable: usize,
    },
}

/// Minimal per-item state for the UI's ring: enough to draw a circular
/// progress indicator and a red error state, with no error text (the full
/// message stays in the `downloads.message` column and the logs).
///
/// Subscribe with [`DownloadQueue::progress`].
#[derive(Debug, Clone)]
pub struct DownloadProgressUpdate {
    /// Row id.
    pub id: String,
    /// Current lifecycle state.
    pub status: DownloadStatus,
    /// Fractional progress in `0.0..=1.0`.
    pub progress: f32,
    /// Whether the row is in the failed (red) state.
    pub failed: bool,
    /// Current phase of a running download.
    pub stage: Option<DownloadStage>,
}

/// Commands handled by the queue actor.
#[derive(Debug)]
pub enum Command {
    /// Enqueue pre-resolved tracks.
    EnqueueResolved(Vec<DownloadRecord>),
    /// Re-enqueue a failed (or paused) row.
    Retry(String),
    /// Re-enqueue every failed row (never `Unavailable` ones).
    RetryFailed,
    /// Abort any in-flight fetch and hold the row as `Paused`.
    Pause(String),
    /// Pause every queued or running row.
    PauseAll,
    /// Flip a `Paused` row back to `Queued` so a worker re-fetches it.
    Resume(String),
    /// Resume every paused row.
    ResumeAll,
    /// Remove a row, aborting an in-flight fetch first.
    Cancel(String),
    /// Cancel every row that is not finished.
    CancelAll,
    /// Delete every `Done` row.
    ClearFinished,
    /// Delete the unlinked downloads (see [`crate::download_cleanup`]).
    CleanUpUnlinked(oneshot::Sender<UnlinkedReport>),
    /// Count the unlinked downloads without deleting anything.
    UnlinkedSummary(oneshot::Sender<UnlinkedReport>),
    /// Queue the album version of a music video (see
    /// [`DownloadQueue::download_album_version`]).
    DownloadAlbumVersion {
        /// Row id of the music video.
        id: String,
        /// Receives the queued song's catalog id, if one was found.
        reply: oneshot::Sender<Option<String>>,
    },
    /// Remove downloaded audio (see [`DownloadQueue::remove_downloads`]).
    RemoveDownloads {
        /// The collection whose download is removed, or `None` for single
        /// songs (which drops every pin of each song).
        owner: Option<String>,
        /// Download keys (catalog ids) concerned.
        track_ids: Vec<String>,
        /// Receives the keys whose audio was deleted.
        reply: oneshot::Sender<Vec<String>>,
    },
    /// Stop every worker and the actor (see [`DownloadQueue::shutdown`]).
    Shutdown {
        /// How long to wait for the workers to stop before aborting them.
        grace: Duration,
        /// Answered once the workers are gone.
        reply: oneshot::Sender<()>,
    },
}

/// Pacing of fetch starts and retries (shortened in tests).
#[derive(Debug, Clone, Copy)]
struct Timing {
    spacing: Duration,
    cooldown: Duration,
    backoff_base: Duration,
    cancel_grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            spacing: START_SPACING,
            cooldown: FAILURE_COOLDOWN,
            backoff_base: BACKOFF_BASE,
            cancel_grace: CANCEL_GRACE,
        }
    }
}

/// Concurrency limiter whose limit can change while downloads run.
///
/// Lowering the limit never interrupts running downloads; new ones wait until
/// the running count drops below it.
struct Gate {
    state: Mutex<GateState>,
    notify: Notify,
}

#[derive(Debug, Clone, Copy)]
struct GateState {
    running: usize,
    limit: usize,
}

/// Releases its slot in the [`Gate`] on drop.
struct GatePermit {
    gate: Arc<Gate>,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(GateState {
                running: 0,
                limit: clamp_concurrency(limit),
            }),
            notify: Notify::new(),
        }
    }

    /// Waits for a free slot.
    async fn acquire(self: &Arc<Self>) -> GatePermit {
        loop {
            // Created before the check: `notify_waiters` reaches every
            // `Notified` that exists when it is called, so no wakeup is lost.
            let notified = self.notify.notified();
            if let Ok(mut state) = self.state.lock()
                && state.running < state.limit
            {
                state.running += 1;
                return GatePermit {
                    gate: Arc::clone(self),
                };
            }
            notified.await;
        }
    }

    fn set_limit(&self, limit: usize) {
        if let Ok(mut state) = self.state.lock() {
            state.limit = clamp_concurrency(limit);
        }
        self.notify.notify_waiters();
    }

    fn limit(&self) -> usize {
        self.state
            .lock()
            .map_or(DEFAULT_CONCURRENCY, |state| state.limit)
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.running = state.running.saturating_sub(1);
        }
        self.gate.notify.notify_waiters();
    }
}

/// Clamps a requested concurrency to `1..=MAX_CONCURRENCY`.
#[must_use]
pub fn clamp_concurrency(limit: usize) -> usize {
    limit.clamp(1, MAX_CONCURRENCY)
}

/// Per-worker switches the actor can flip while the worker runs.
#[derive(Debug, Clone)]
struct WorkerFlags {
    /// Re-download: fetch every wanted mix again, even ones already on disk.
    replace: Arc<AtomicBool>,
    /// A re-download was merged into this worker after it planned its mixes;
    /// it fetches the skipped ones once it is done.
    upgraded: Arc<AtomicBool>,
}

impl WorkerFlags {
    fn new(replace: bool) -> Self {
        Self {
            replace: Arc::new(AtomicBool::new(replace)),
            upgraded: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The worker of one row.
struct Worker {
    /// Generation handed out when it was spawned.
    epoch: u64,
    /// Cancels its fetch (pause / cancel / replacement).
    token: CancellationToken,
    /// Its task, awaited by the next worker for the same row.
    handle: Option<JoinHandle<()>>,
    flags: WorkerFlags,
}

/// Shared state between the actor and its worker tasks.
struct Inner {
    storage: Storage,
    downloaders: Arc<DownloaderRegistry>,
    media_dir: PathBuf,
    /// Catalog search used to find the twin of a library-only item.
    search: Arc<dyn TwinSearch>,
    /// Catalog lookup of a song's spatial stream.
    spatial: Arc<dyn SpatialLookup>,
    /// [`DownloadFormat::as_u8`] of the current download format.
    format: Arc<AtomicU8>,
    /// Post-download lyrics step; `None` disables it.
    lyrics: Option<Arc<dyn LyricsFetcher>>,
    gate: Arc<Gate>,
    timing: Timing,
    /// Earliest instant the next fetch may start (start spacing + cooldown).
    next_start: Mutex<Instant>,
    /// Moving estimate of one fetch's duration, for estimated progress.
    fetch_estimate_ms: AtomicU64,
    events: broadcast::Sender<DownloadEvent>,
    progress_events: broadcast::Sender<DownloadProgressUpdate>,
    /// Live workers by row id (at most one per song: the row id is the
    /// download key). Pausing or cancelling a row cancels its worker's token;
    /// the entry stays until the worker has exited, so the next worker for
    /// the same id can wait for it. A worker only removes its own entry (the
    /// stored epoch still matches), and its row writes only land while it is
    /// current (see [`write_guard`]).
    active: Arc<Mutex<HashMap<String, Worker>>>,
    /// Monotonic generation counter handed to each spawned worker.
    epoch: AtomicU64,
    /// Deferred row ids waiting for the idle window before a worker starts.
    pending_deferred: Mutex<HashSet<String>>,
    /// Unix milliseconds of the most recent playback event, written by the
    /// auto-cache hook and read by the actor to decide when deferred rows
    /// may run.
    last_played_ts: Arc<AtomicU64>,
}

impl Inner {
    /// Whether playback has been quiet long enough for deferred rows to run.
    fn is_idle(&self) -> bool {
        let last = self.last_played_ts.load(Ordering::Relaxed);
        if last == 0 {
            return true;
        }
        let now = crate::playback::now_unix_ms();
        now.saturating_sub(last) >= AUTO_CACHE_IDLE_SECS * 1_000
    }

    /// Reserves the next fetch start time (at least [`START_SPACING`] after
    /// the previous one) and returns it.
    fn reserve_start(&self) -> Instant {
        let now = Instant::now();
        let Ok(mut next) = self.next_start.lock() else {
            return now;
        };
        let at = (*next).max(now);
        *next = at + self.timing.spacing;
        at
    }

    /// Holds back every new fetch start for [`FAILURE_COOLDOWN`], or for
    /// Apple's `Retry-After` when that is longer (it applies to the account,
    /// not one track).
    fn cool_down(&self, retry_after: Option<Duration>) {
        let pause = retry_after.map_or(self.timing.cooldown, |after| {
            after.min(RETRY_AFTER_MAX).max(self.timing.cooldown)
        });
        if let Ok(mut next) = self.next_start.lock() {
            *next = (*next).max(Instant::now() + pause);
        }
    }

    /// What the registered downloader supports (nothing without one).
    fn capabilities(&self) -> DownloaderCapabilities {
        self.downloaders
            .get(DOWNLOADER_ID)
            .map(|downloader| downloader.capabilities())
            .unwrap_or_default()
    }

    fn download_format(&self) -> DownloadFormat {
        DownloadFormat::from_u8(self.format.load(Ordering::Relaxed))
    }

    fn fetch_estimate(&self) -> Duration {
        Duration::from_millis(self.fetch_estimate_ms.load(Ordering::Relaxed))
    }

    /// Folds a finished fetch's duration into the moving estimate.
    fn record_fetch_time(&self, took: Duration) {
        let took = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        let old = self.fetch_estimate_ms.load(Ordering::Relaxed);
        self.fetch_estimate_ms
            .store((old.saturating_mul(3) + took) / 4, Ordering::Relaxed);
    }
}

/// Handle to the download queue. Cheap to clone; all clones share the actor.
#[derive(Clone)]
pub struct DownloadQueue {
    cmd_tx: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<DownloadEvent>,
    progress_events: broadcast::Sender<DownloadProgressUpdate>,
    gate: Arc<Gate>,
    format: Arc<AtomicU8>,
}

impl std::fmt::Debug for DownloadQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadQueue")
            .field("concurrency", &self.gate.limit())
            .field("format", &self.download_format())
            .finish_non_exhaustive()
    }
}

impl DownloadQueue {
    /// Spawns the queue actor on the current tokio runtime with
    /// [`DEFAULT_CONCURRENCY`] (change it with [`Self::set_concurrency`]) and
    /// the [`DownloadFormat::Stereo`] format (change it with
    /// [`Self::set_download_format`]).
    ///
    /// Any non-deferred rows that were `Queued`, `Downloading` or `Paused` at
    /// startup are reset to `Queued` and re-dispatched immediately; deferred
    /// rows are reset to `Queued` and wait for the idle window. Audio files
    /// already in the media directory are linked to tracks that miss them.
    ///
    /// When `lyrics` is set, every successfully downloaded track then gets its
    /// lyrics fetched through it (see [`LyricsFetcher`]).
    #[must_use]
    pub fn spawn(
        storage: Storage,
        downloaders: Arc<DownloaderRegistry>,
        media_dir: PathBuf,
        last_played_ts: Arc<AtomicU64>,
        lyrics: Option<Arc<dyn LyricsFetcher>>,
    ) -> Self {
        let catalog = Arc::new(CatalogClient::new(storage.clone()));
        Self::spawn_with(
            storage,
            downloaders,
            media_dir,
            last_played_ts,
            lyrics,
            Timing::default(),
            catalog,
        )
    }

    fn spawn_with<C: TwinSearch + SpatialLookup + 'static>(
        storage: Storage,
        downloaders: Arc<DownloaderRegistry>,
        media_dir: PathBuf,
        last_played_ts: Arc<AtomicU64>,
        lyrics: Option<Arc<dyn LyricsFetcher>>,
        timing: Timing,
        catalog: Arc<C>,
    ) -> Self {
        let (events, _rx) = broadcast::channel(EVENTS_CAPACITY);
        let (progress_events, _prx) = broadcast::channel(PROGRESS_CAPACITY);
        let gate = Arc::new(Gate::new(DEFAULT_CONCURRENCY));
        let format = Arc::new(AtomicU8::new(DownloadFormat::Stereo.as_u8()));
        let search: Arc<dyn TwinSearch> = Arc::clone(&catalog) as Arc<dyn TwinSearch>;
        let inner = Arc::new(Inner {
            storage,
            downloaders,
            media_dir,
            search,
            spatial: catalog,
            format: Arc::clone(&format),
            lyrics,
            gate: Arc::clone(&gate),
            timing,
            next_start: Mutex::new(Instant::now()),
            fetch_estimate_ms: AtomicU64::new(INITIAL_FETCH_ESTIMATE_MS),
            events: events.clone(),
            progress_events: progress_events.clone(),
            active: Arc::new(Mutex::new(HashMap::new())),
            epoch: AtomicU64::new(0),
            pending_deferred: Mutex::new(HashSet::new()),
            last_played_ts,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_actor(Arc::clone(&inner), cmd_rx));
        Self {
            cmd_tx,
            events,
            progress_events,
            gate,
            format,
        }
    }

    /// Sets which versions of a song downloads fetch. Applies to every fetch
    /// that starts afterwards.
    pub fn set_download_format(&self, format: DownloadFormat) {
        self.format.store(format.as_u8(), Ordering::Relaxed);
        tracing::info!(?format, "download format set");
    }

    /// The current download format.
    #[must_use]
    pub fn download_format(&self) -> DownloadFormat {
        DownloadFormat::from_u8(self.format.load(Ordering::Relaxed))
    }

    /// Sets how many downloads run at once (clamped to
    /// `1..=`[`MAX_CONCURRENCY`]). Applies immediately: a higher limit starts
    /// waiting downloads, a lower one lets running downloads finish first.
    pub fn set_concurrency(&self, limit: usize) {
        let limit = clamp_concurrency(limit);
        self.gate.set_limit(limit);
        tracing::info!(limit, "download concurrency set");
    }

    /// Current simultaneous-download limit.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.gate.limit()
    }

    /// Subscribes to progress events. Events emitted before the subscription
    /// are not replayed.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<DownloadEvent> {
        self.events.subscribe()
    }

    /// Subscribes to the minimal per-item progress stream the UI draws the
    /// ring / red state from (`{id, status, progress, failed}`, no error text).
    /// Updates emitted before the subscription are not replayed.
    #[must_use]
    pub fn progress(&self) -> broadcast::Receiver<DownloadProgressUpdate> {
        self.progress_events.subscribe()
    }

    /// Enqueues pre-resolved tracks. Rows that are already queued or running
    /// keep running (their new owner is still recorded); music videos are
    /// recorded as [`DownloadStatus::Unavailable`] without contacting Apple
    /// while the downloader does not declare
    /// [`DownloaderCapabilities::music_videos`]. A library-only item is
    /// resolved to its playable twin when its fetch starts.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn enqueue(&self, records: Vec<DownloadRecord>) -> Result<(), QueueError> {
        self.send(Command::EnqueueResolved(records))
    }

    /// Flips a `Failed` or `Paused` row back to `Queued` and re-dispatches it.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn retry(&self, id: &str) -> Result<(), QueueError> {
        self.send(Command::Retry(id.to_owned()))
    }

    /// Retries every failed row. `Unavailable` rows are never retried.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn retry_failed(&self) -> Result<(), QueueError> {
        self.send(Command::RetryFailed)
    }

    /// Pauses a row: aborts its in-flight fetch if one is running and marks it
    /// `Paused` (it is not retried automatically). A queued row that has not
    /// started yet is simply held.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn pause(&self, id: &str) -> Result<(), QueueError> {
        self.send(Command::Pause(id.to_owned()))
    }

    /// Pauses every queued or running row.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn pause_all(&self) -> Result<(), QueueError> {
        self.send(Command::PauseAll)
    }

    /// Resumes a `Paused` row by flipping it back to `Queued`; a worker then
    /// re-fetches it from scratch.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn resume(&self, id: &str) -> Result<(), QueueError> {
        self.send(Command::Resume(id.to_owned()))
    }

    /// Resumes every paused row.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn resume_all(&self) -> Result<(), QueueError> {
        self.send(Command::ResumeAll)
    }

    /// Cancels a row and removes it from the queue, aborting an in-flight fetch
    /// first. Already-finished rows are deleted; unknown rows are ignored.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn cancel(&self, id: &str) -> Result<(), QueueError> {
        self.send(Command::Cancel(id.to_owned()))
    }

    /// Cancels every unfinished row (queued, running, paused, failed or
    /// "can't download yet"), as [`Self::cancel`] does for one.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn cancel_all(&self) -> Result<(), QueueError> {
        self.send(Command::CancelAll)
    }

    /// Deletes the unlinked downloads: files no download pin needs any more
    /// (see [`crate::download_cleanup`]). Stale pins of downloaded
    /// collections are dropped first. Custom audio, files a custom song plays
    /// by reference, links to local files and unfinished downloads are never
    /// touched. Returns what was deleted.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub async fn clean_up_unlinked(&self) -> Result<UnlinkedReport, QueueError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::CleanUpUnlinked(reply))?;
        wait.await.map_err(|_| QueueError::Closed)
    }

    /// Counts the unlinked downloads (and their size) without deleting
    /// anything; see [`Self::clean_up_unlinked`].
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub async fn unlinked_downloads(&self) -> Result<UnlinkedReport, QueueError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::UnlinkedSummary(reply))?;
        wait.await.map_err(|_| QueueError::Closed)
    }

    /// Deletes every row with status `Done`.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub fn clear_finished(&self) -> Result<(), QueueError> {
        self.send(Command::ClearFinished)
    }

    /// Queues the album version of the music video in row `id`: the audio
    /// song with the same title and artist on the same album (see
    /// [`crate::album_version`]). It is a separate row keyed on the song's own
    /// catalog id; the video row stays "Can't download yet". Returns the
    /// queued catalog id, or `None` when no album version is known.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub async fn download_album_version(&self, id: &str) -> Result<Option<String>, QueueError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::DownloadAlbumVersion {
            id: id.to_owned(),
            reply,
        })?;
        wait.await.map_err(|_| QueueError::Closed)
    }

    /// Removes downloaded audio, Apple Music style.
    ///
    /// With `owner = Some(collection)`, drops that collection's pins on
    /// `track_ids` and deletes only the files no other downloaded collection
    /// and no individual download still needs. With `owner = None` (single
    /// songs), every pin of each track is dropped and its file deleted.
    /// In-flight downloads of deleted tracks are cancelled and their queue
    /// rows removed. Track mappings to user files are never touched. Returns
    /// the download keys whose audio was deleted.
    ///
    /// # Errors
    ///
    /// [`QueueError::Closed`] if the actor has stopped.
    pub async fn remove_downloads(
        &self,
        owner: Option<&str>,
        track_ids: Vec<String>,
    ) -> Result<Vec<String>, QueueError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::RemoveDownloads {
            owner: owner.map(str::to_owned),
            track_ids,
            reply,
        })?;
        wait.await.map_err(|_| QueueError::Closed)
    }

    /// Stops the queue: cancels every running download, waits up to `grace`
    /// for the workers to exit (aborting the rest), and ends the actor. Rows
    /// keep their stored state, so the next queue over the same store
    /// resumes them. Later commands fail with [`QueueError::Closed`].
    pub async fn shutdown(&self, grace: Duration) {
        let (reply, done) = oneshot::channel();
        if self.send(Command::Shutdown { grace, reply }).is_ok() {
            let _ = done.await;
        }
    }

    fn send(&self, cmd: Command) -> Result<(), QueueError> {
        self.cmd_tx.send(cmd).map_err(|_| QueueError::Closed)
    }
}

async fn run_actor(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Command>) {
    delete_stale_temp_files(&inner.media_dir).await;
    crate::media::link_existing_media(&inner.storage, &inner.media_dir).await;
    // Resume any rows left over from a previous run.
    match inner.storage.list_downloads().await {
        Ok(existing) => {
            for mut record in existing {
                match record.status {
                    DownloadStatus::Queued
                    | DownloadStatus::Downloading
                    | DownloadStatus::Paused => {
                        record.status = DownloadStatus::Queued;
                        record.progress = 0.0;
                        record.message = None;
                        if let Err(error) = inner
                            .storage
                            .update_download_status(&record.id, DownloadStatus::Queued, 0.0, None)
                            .await
                        {
                            tracing::warn!(id = %record.id, %error, "could not reset download row");
                            continue;
                        }
                        emit_status(&inner, &record.id, DownloadStatus::Queued, 0.0, None);
                        dispatch(&inner, record);
                    }
                    DownloadStatus::Done | DownloadStatus::Failed | DownloadStatus::Unavailable => {
                    }
                }
            }
            emit_summary(&inner).await;
        }
        Err(error) => {
            tracing::warn!(%error, "could not load existing downloads");
        }
    }

    let mut tick = tokio::time::interval(DEFERRED_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                if let Command::Shutdown { grace, reply } = cmd {
                    stop_workers(&inner, grace).await;
                    let _ = reply.send(());
                    break;
                }
                handle_command(&inner, cmd).await;
                emit_summary(&inner).await;
            }
            _ = tick.tick() => {
                drain_deferred_if_idle(&inner).await;
            }
        }
    }
    tracing::debug!("download queue actor stopped");
}

/// Cancels every worker and waits up to `grace` for them to exit (then
/// aborts the rest). The workers' entries are removed first, so none of them
/// writes its row again: rows that were running stay as they are in the
/// store and are queued again when the next queue starts.
async fn stop_workers(inner: &Arc<Inner>, grace: Duration) {
    if let Ok(mut set) = inner.pending_deferred.lock() {
        set.clear();
    }
    let handles: Vec<JoinHandle<()>> = match inner.active.lock() {
        Ok(mut map) => map
            .drain()
            .filter_map(|(_, worker)| {
                worker.token.cancel();
                worker.handle
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let count = handles.len();
    let deadline = Instant::now() + grace;
    for mut handle in handles {
        let left = deadline.saturating_duration_since(Instant::now());
        if tokio::time::timeout(left, &mut handle).await.is_err() {
            tracing::warn!("download worker did not stop in time; aborting it");
            handle.abort();
            let _ = handle.await;
        }
    }
    tracing::info!(workers = count, "download queue stopped");
}

async fn handle_command(inner: &Arc<Inner>, cmd: Command) {
    match cmd {
        // Handled by the actor loop, which stops afterwards.
        Command::Shutdown { reply, .. } => {
            let _ = reply.send(());
        }
        Command::EnqueueResolved(records) => {
            for record in records {
                handle_enqueue(inner, record).await;
            }
            drain_deferred_if_idle(inner).await;
        }
        Command::Retry(id) => {
            handle_retry(inner, &id).await;
            drain_deferred_if_idle(inner).await;
        }
        Command::RetryFailed => {
            for id in ids_with_status(inner, &[DownloadStatus::Failed]).await {
                handle_retry(inner, &id).await;
            }
            drain_deferred_if_idle(inner).await;
        }
        Command::Pause(id) => handle_pause(inner, &id).await,
        Command::PauseAll => {
            let live = [DownloadStatus::Queued, DownloadStatus::Downloading];
            for id in ids_with_status(inner, &live).await {
                handle_pause(inner, &id).await;
            }
        }
        Command::Resume(id) => {
            handle_resume(inner, &id).await;
            drain_deferred_if_idle(inner).await;
        }
        Command::ResumeAll => {
            for id in ids_with_status(inner, &[DownloadStatus::Paused]).await {
                handle_resume(inner, &id).await;
            }
            drain_deferred_if_idle(inner).await;
        }
        Command::Cancel(id) => handle_cancel(inner, &id).await,
        Command::CancelAll => {
            let unfinished = [
                DownloadStatus::Queued,
                DownloadStatus::Downloading,
                DownloadStatus::Paused,
                DownloadStatus::Failed,
                DownloadStatus::Unavailable,
            ];
            let ids = ids_with_status(inner, &unfinished).await;
            // Stop every worker first, so they wind down together.
            for id in &ids {
                cancel_active(inner, id);
            }
            for id in ids {
                handle_cancel(inner, &id).await;
            }
        }
        Command::CleanUpUnlinked(reply) => {
            let _ = reply.send(handle_clean_up_unlinked(inner).await);
        }
        Command::UnlinkedSummary(reply) => {
            let report =
                match crate::download_cleanup::unlinked_downloads(&inner.storage, &inner.media_dir)
                    .await
                {
                    Ok((_, report)) => report,
                    Err(error) => {
                        tracing::warn!(%error, "could not look for unlinked downloads");
                        UnlinkedReport::default()
                    }
                };
            let _ = reply.send(report);
        }
        Command::ClearFinished => handle_clear_finished(inner).await,
        Command::DownloadAlbumVersion { id, reply } => {
            let queued = handle_album_version(inner, &id).await;
            let _ = reply.send(queued);
            drain_deferred_if_idle(inner).await;
        }
        Command::RemoveDownloads {
            owner,
            track_ids,
            reply,
        } => {
            let removed = handle_remove_downloads(inner, owner.as_deref(), track_ids).await;
            let _ = reply.send(removed);
        }
    }
}

/// Ids of every row whose status is one of `statuses`.
async fn ids_with_status(inner: &Arc<Inner>, statuses: &[DownloadStatus]) -> Vec<String> {
    match inner.storage.list_downloads().await {
        Ok(rows) => rows
            .into_iter()
            .filter(|row| statuses.contains(&row.status))
            .map(|row| row.id)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "could not list downloads");
            Vec::new()
        }
    }
}

/// Dispatches a record: spawns a worker immediately for non-deferred rows,
/// or parks it in `pending_deferred` for the next idle window.
fn dispatch(inner: &Arc<Inner>, record: DownloadRecord) {
    if record.deferred {
        if let Ok(mut set) = inner.pending_deferred.lock() {
            set.insert(record.id.clone());
        }
    } else {
        spawn_worker(inner, record);
    }
}

/// Spawns the worker of `record`. A previous worker of the same row is
/// cancelled and replaced; the new one waits until it has fully stopped
/// before it fetches anything, so two downloads of one song never overlap.
fn spawn_worker(inner: &Arc<Inner>, record: DownloadRecord) {
    let Ok(mut map) = inner.active.lock() else {
        return;
    };
    let previous = map.remove(&record.id).and_then(|worker| {
        worker.token.cancel();
        worker.handle
    });
    let token = CancellationToken::new();
    let epoch = inner.epoch.fetch_add(1, Ordering::Relaxed);
    let flags = WorkerFlags::new(record.replace);
    let id = record.id.clone();
    // Spawned while the map is locked: the worker removes its own entry when
    // it ends, which can then only happen after the entry exists.
    let handle = tokio::spawn(run_worker(
        Arc::clone(inner),
        record,
        token.clone(),
        epoch,
        previous,
        flags.clone(),
    ));
    map.insert(
        id,
        Worker {
            epoch,
            token,
            handle: Some(handle),
            flags,
        },
    );
}

/// Removes `id` from the active map only when the stored generation still
/// matches `epoch`, so a worker never evicts a newer re-dispatch of the same id.
fn clear_active(inner: &Arc<Inner>, id: &str, epoch: u64) {
    if let Ok(mut map) = inner.active.lock()
        && map.get(id).is_some_and(|worker| worker.epoch == epoch)
    {
        map.remove(id);
    }
}

/// Cancels the worker of `id`, if any. Its entry stays until it has exited,
/// so a later worker for the row waits for it.
fn cancel_active(inner: &Arc<Inner>, id: &str) {
    if let Ok(map) = inner.active.lock()
        && let Some(worker) = map.get(id)
    {
        worker.token.cancel();
    }
}

/// Waits until the worker of `id` (already cancelled) has exited and removed
/// its entry, for at most the cancel grace plus a second.
async fn wait_for_worker_exit(inner: &Arc<Inner>, id: &str) {
    let deadline = Instant::now() + inner.timing.cancel_grace + Duration::from_secs(1);
    loop {
        let running = inner.active.lock().is_ok_and(|map| map.contains_key(id));
        if !running {
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(%id, "cancelled download worker is still running");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The switches of the live (not cancelled) worker of `id`, if one runs.
fn live_worker(inner: &Inner, id: &str) -> Option<WorkerFlags> {
    let map = inner.active.lock().ok()?;
    map.get(id)
        .filter(|worker| !worker.token.is_cancelled())
        .map(|worker| worker.flags.clone())
}

/// Whether the worker spawned as `epoch` is still the current, uncancelled
/// worker of `id`.
fn is_current(active: &Mutex<HashMap<String, Worker>>, id: &str, epoch: u64) -> bool {
    active.lock().is_ok_and(|map| {
        map.get(id)
            .is_some_and(|worker| worker.epoch == epoch && !worker.token.is_cancelled())
    })
}

/// The check a worker's row write runs with the database locked: the write
/// lands only while the worker is current. The actor cancels a worker
/// (under the map lock) before it writes the row itself, so a late worker
/// write can never overwrite "Paused" or resurrect a cancelled row.
fn write_guard(inner: &Inner, id: &str, epoch: u64) -> impl FnOnce() -> bool + Send + 'static {
    let active = Arc::clone(&inner.active);
    let id = id.to_owned();
    move || is_current(&active, &id, epoch)
}

/// Publishes a worker's status change if the worker is still current. The
/// map stays locked while publishing, so the event can't overtake the
/// actor's own event of a pause or cancel (the actor cancels under the same
/// lock before it publishes). Returns whether it was published.
#[expect(
    clippy::too_many_arguments,
    reason = "the event fields plus the worker identity it is guarded by"
)]
fn emit_if_current(
    inner: &Arc<Inner>,
    id: &str,
    epoch: u64,
    status: DownloadStatus,
    progress: f32,
    stage: Option<DownloadStage>,
    attempt: u32,
    message: Option<String>,
) -> bool {
    let Ok(map) = inner.active.lock() else {
        return false;
    };
    let current = map
        .get(id)
        .is_some_and(|worker| worker.epoch == epoch && !worker.token.is_cancelled());
    if current {
        emit_live(inner, id, status, progress, stage, attempt, message);
    }
    current
}

/// Persists and publishes a worker's status change, only while the worker
/// is current. Returns whether it was written.
#[expect(
    clippy::too_many_arguments,
    reason = "the row update plus the worker identity it is guarded by"
)]
async fn worker_status(
    inner: &Arc<Inner>,
    id: &str,
    epoch: u64,
    status: DownloadStatus,
    progress: f32,
    stage: Option<DownloadStage>,
    attempt: u32,
    message: Option<String>,
) -> bool {
    match inner
        .storage
        .update_download_status_if(
            id,
            status,
            progress,
            message.clone(),
            write_guard(inner, id, epoch),
        )
        .await
    {
        Ok(true) => emit_if_current(inner, id, epoch, status, progress, stage, attempt, message),
        Ok(false) => false,
        Err(error) => {
            tracing::warn!(%id, %error, ?status, "could not record download status");
            false
        }
    }
}

/// If the idle window is open, drains the deferred backlog and spawns a
/// worker for each row. Workers are bounded by the concurrency gate, so a
/// large backlog does not overwhelm the downloader.
async fn drain_deferred_if_idle(inner: &Arc<Inner>) {
    if !inner.is_idle() {
        return;
    }
    let ids: Vec<String> = match inner.pending_deferred.lock() {
        Ok(mut set) => set.drain().collect(),
        Err(_) => return,
    };
    for id in ids {
        match inner.storage.get_download(&id).await {
            Ok(Some(record)) if record.status == DownloadStatus::Queued => {
                spawn_worker(inner, record);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%id, %error, "could not load deferred download row");
            }
        }
    }
}

/// The owner a record pins its track for: its album / playlist, or `None`
/// for an individual (single-song / auto-cache) download.
fn pin_owner(record: &DownloadRecord) -> Option<&str> {
    match record.source_kind {
        DownloadSourceKind::Album | DownloadSourceKind::Playlist => record.source_id.as_deref(),
        DownloadSourceKind::Song => None,
    }
}

/// Why the downloader cannot handle a track yet, decided from what the store
/// knows, without contacting Apple. `None` when it may be downloadable.
async fn unavailable_reason(inner: &Inner, track_id: &str) -> Option<String> {
    if inner.capabilities().music_videos {
        return None;
    }
    let rows = inner.storage.tracks_by_any_id(track_id).await.ok()?;
    rows.iter()
        .any(|row| {
            row.track
                .metadata
                .as_deref()
                .is_some_and(download_target::is_music_video)
        })
        .then(|| MUSIC_VIDEO_REASON.to_owned())
}

/// See [`DownloadQueue::download_album_version`].
async fn handle_album_version(inner: &Arc<Inner>, id: &str) -> Option<String> {
    let video_id = match inner.storage.get_download(id).await {
        Ok(Some(row)) => row.track_id,
        _ => id.to_owned(),
    };
    let version = match download_target::album_version(&inner.storage, &video_id).await {
        Ok(Some(version)) => version,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%id, %error, "could not look up the album version");
            return None;
        }
    };
    tracing::info!(video = %video_id, song = %version.catalog_id, "album version requested");
    let record = DownloadRecord {
        id: version.catalog_id.clone(),
        track_id: version.catalog_id.clone(),
        title: version.title,
        artist: version.artist,
        album: version.album,
        duration_ms: version.duration_ms.unwrap_or(0),
        artwork_url: version.artwork_url,
        source_kind: DownloadSourceKind::Song,
        source_id: None,
        source_name: None,
        status: DownloadStatus::Queued,
        progress: 0.0,
        message: None,
        deferred: false,
        replace: false,
        created_at: 0,
        updated_at: 0,
    };
    handle_enqueue(inner, record).await;
    Some(version.catalog_id)
}

/// Queues `record`, merging it into the row the song already has: the
/// queue holds one row per download key (song), covering every mix the
/// download format wants, so it never runs two downloads of one song.
///
/// - Queued or running: kept running. A re-download upgrades it to
///   replace existing files; a manual download of a deferred auto-cache row
///   starts it now.
/// - Paused: resumed (with the merged flags).
/// - Finished, failed, unavailable or new: queued afresh.
async fn handle_enqueue(inner: &Arc<Inner>, mut record: DownloadRecord) {
    let existing = match inner.storage.get_download(&record.id).await {
        Ok(existing) => existing,
        Err(error) => {
            tracing::warn!(id = %record.id, %error, "could not read download row");
            None
        }
    };
    if let Some(reason) = unavailable_reason(inner, &record.track_id).await {
        record.status = DownloadStatus::Unavailable;
        record.progress = 0.0;
        record.message = Some(reason);
        if let Err(error) = inner.storage.upsert_download(record.clone()).await {
            tracing::warn!(id = %record.id, %error, "could not insert download row");
            return;
        }
        tracing::info!(id = %record.id, reason = ?record.message, "download not supported yet");
        emit(&inner.events, DownloadEvent::Added(record));
        return;
    }
    if let Err(error) = inner
        .storage
        .pin_downloads(pin_owner(&record), vec![record.track_id.clone()])
        .await
    {
        tracing::warn!(id = %record.id, %error, "could not pin download");
    }
    let next = match existing {
        Some(existing)
            if matches!(
                existing.status,
                DownloadStatus::Queued | DownloadStatus::Downloading
            ) =>
        {
            merge_into_live(inner, existing, &record).await;
            return;
        }
        // Resume the paused row; it keeps its place and source.
        Some(mut existing) if existing.status == DownloadStatus::Paused => {
            existing.replace |= record.replace;
            existing.deferred &= record.deferred;
            existing
        }
        _ => record,
    };
    let mut next = next;
    next.status = DownloadStatus::Queued;
    next.progress = 0.0;
    next.message = None;
    tracing::debug!(id = %next.id, replace = next.replace, "download queued");
    if let Err(error) = inner.storage.upsert_download(next.clone()).await {
        tracing::warn!(id = %next.id, %error, "could not insert download row");
        return;
    }
    emit(&inner.events, DownloadEvent::Added(next.clone()));
    dispatch(inner, next);
}

/// Merges a new request into a queued or running row (see
/// [`handle_enqueue`]).
async fn merge_into_live(inner: &Arc<Inner>, existing: DownloadRecord, incoming: &DownloadRecord) {
    let id = existing.id.clone();
    let replace = existing.replace || incoming.replace;
    let deferred = existing.deferred && incoming.deferred;
    if (replace != existing.replace || deferred != existing.deferred)
        && let Err(error) = inner
            .storage
            .set_download_flags(&id, replace, deferred)
            .await
    {
        tracing::warn!(%id, %error, "could not update download flags");
    }
    let worker = live_worker(inner, &id);
    // Re-inserted below when it stays parked.
    if let Ok(mut set) = inner.pending_deferred.lock() {
        set.remove(&id);
    }
    let undeferred = existing.deferred && !deferred;
    let running = existing.status == DownloadStatus::Downloading;
    let mut next = existing;
    next.replace = replace;
    next.deferred = deferred;
    match worker {
        // Started already: it checked `deferred` before it began.
        Some(flags) if running || !undeferred => {
            if replace && !flags.replace.swap(true, Ordering::SeqCst) {
                flags.upgraded.store(true, Ordering::SeqCst);
                tracing::info!(%id, "re-download merged into the running download");
            }
        }
        // A deferred row the user now wants at once, or a row nobody works
        // on (left over by a crash): start a worker now.
        _ if undeferred || !next.deferred => {
            tracing::debug!(%id, undeferred, "starting a worker for the merged download");
            spawn_worker(inner, next);
        }
        // Still deferred and parked: leave it for the idle window.
        _ => {
            if let Ok(mut set) = inner.pending_deferred.lock() {
                set.insert(id);
            }
        }
    }
}

async fn handle_retry(inner: &Arc<Inner>, id: &str) {
    let record = match inner.storage.get_download(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%id, %error, "could not read download row");
            return;
        }
    };
    if !matches!(
        record.status,
        DownloadStatus::Failed | DownloadStatus::Paused
    ) {
        return;
    }
    if let Err(error) = inner
        .storage
        .update_download_status(id, DownloadStatus::Queued, 0.0, None)
        .await
    {
        tracing::warn!(%id, %error, "could not reset download row for retry");
        return;
    }
    emit_status(inner, id, DownloadStatus::Queued, 0.0, None);
    let mut next = record;
    next.status = DownloadStatus::Queued;
    next.progress = 0.0;
    next.message = None;
    dispatch(inner, next);
}

/// Cancels a row: stops its worker (a fetch in progress is abandoned and
/// its temporary file deleted by the downloader), removes the row, and,
/// when the song has no downloaded mix, drops the pins that only asked for
/// it (a kept collection's pin stays). Files that were already complete are
/// never deleted.
async fn handle_cancel(inner: &Arc<Inner>, id: &str) {
    // Abort an in-flight (or slot-waiting) worker, and drop a deferred row
    // that has not been dispatched yet so the next drain cannot resurrect it.
    cancel_active(inner, id);
    if let Ok(mut set) = inner.pending_deferred.lock() {
        set.remove(id);
    }
    // A fetch that completes while it winds down still records its file.
    // Wait for that first, so the pins below (and a "Remove downloads"
    // deleting files after this) see the final state instead of having the
    // file recorded again behind their back.
    wait_for_worker_exit(inner, id).await;
    let row = inner.storage.get_download(id).await.ok().flatten();
    match inner.storage.delete_download(id).await {
        Ok(true) => emit(&inner.events, DownloadEvent::Removed(id.to_owned())),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%id, %error, "could not delete download");
        }
    }
    if let Some(row) = row {
        release_unfetched_pins(inner, &row.track_id).await;
    }
}

/// Drops the pins of a song that has no downloaded mix, except the pins of
/// collections set to "Keep downloaded", and forgets collections left with
/// no pin at all.
async fn release_unfetched_pins(inner: &Arc<Inner>, key: &str) {
    let has_audio = inner
        .storage
        .local_variants(vec![key.to_owned()])
        .await
        .map_or(true, |found| found.contains_key(key));
    if has_audio {
        return;
    }
    let (Ok(collections), Ok(pins)) = (
        inner.storage.download_collections().await,
        inner.storage.download_pins().await,
    ) else {
        return;
    };
    let kept: HashSet<&str> = collections
        .iter()
        .filter(|c| c.keep)
        .map(|c| c.owner.as_str())
        .collect();
    let drop: Vec<(String, String)> = pins
        .iter()
        .filter(|(track, owner)| track == key && !kept.contains(owner.as_str()))
        .cloned()
        .collect();
    if drop.is_empty() {
        return;
    }
    if let Err(error) = inner.storage.delete_download_pins(drop.clone()).await {
        tracing::warn!(%key, %error, "could not release the pins of a cancelled download");
        return;
    }
    let left: HashSet<&str> = pins
        .iter()
        .filter(|pin| !drop.contains(pin))
        .map(|(_, owner)| owner.as_str())
        .collect();
    forget_unpinned_collections(inner, &collections, &left).await;
}

/// Forgets every registered collection that is not kept and has no pin in
/// `owners_with_pins` any more.
async fn forget_unpinned_collections(
    inner: &Inner,
    collections: &[crate::storage::DownloadCollection],
    owners_with_pins: &HashSet<&str>,
) {
    for collection in collections {
        if !collection.keep
            && !owners_with_pins.contains(collection.owner.as_str())
            && let Err(error) = inner
                .storage
                .unregister_download_collection(&collection.owner)
                .await
        {
            tracing::warn!(owner = %collection.owner, %error, "could not forget the collection");
        }
    }
}

/// See [`DownloadQueue::clean_up_unlinked`].
async fn handle_clean_up_unlinked(inner: &Arc<Inner>) -> UnlinkedReport {
    let scan = match crate::download_cleanup::scan(&inner.storage, &inner.media_dir).await {
        Ok(scan) => scan,
        Err(error) => {
            tracing::warn!(%error, "could not look for unlinked downloads");
            return UnlinkedReport::default();
        }
    };
    if !scan.stale_pins.is_empty() {
        tracing::info!(
            count = scan.stale_pins.len(),
            "dropping stale download pins"
        );
        if let Err(error) = inner
            .storage
            .delete_download_pins(scan.stale_pins.clone())
            .await
        {
            tracing::warn!(%error, "could not drop stale download pins");
            return UnlinkedReport::default();
        }
        if let (Ok(collections), Ok(pins)) = (
            inner.storage.download_collections().await,
            inner.storage.download_pins().await,
        ) {
            let owners: HashSet<&str> = pins.iter().map(|(_, owner)| owner.as_str()).collect();
            forget_unpinned_collections(inner, &collections, &owners).await;
        }
    }
    let mut report = UnlinkedReport::default();
    let mut removed: Vec<String> = Vec::new();
    for file in scan.files {
        match tokio::fs::remove_file(&file.path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %file.path.display(), %error, "could not delete an unlinked download");
                continue;
            }
        }
        if let Err(error) = inner.storage.delete_audio_path(&file.stored).await {
            tracing::warn!(path = %file.path.display(), %error, "could not forget an unlinked download");
            continue;
        }
        report.files += 1;
        report.bytes += file.size;
        for key in file.keys {
            if !removed.contains(&key) {
                removed.push(key);
            }
        }
    }
    if report.files > 0 {
        tracing::info!(
            files = report.files,
            bytes = report.bytes,
            "unlinked downloads cleaned up"
        );
        forget_finished_rows(inner, &removed).await;
        emit(&inner.events, DownloadEvent::AudioRemoved(removed));
    }
    report
}

/// Ids of the queue rows in `rows` that download song `key` under another
/// row id (a row keyed on the song itself is `key`).
fn rows_of_song<'a>(rows: &'a [DownloadRecord], key: &'a str) -> impl Iterator<Item = &'a str> {
    rows.iter()
        .filter(move |row| row.track_id == key && row.id != key)
        .map(|row| row.id.as_str())
}

/// Deletes the finished queue rows of the songs `keys` that have no
/// downloaded mix left, so songs whose files were deleted do not linger in
/// the Downloads panel as "Downloaded".
async fn forget_finished_rows(inner: &Arc<Inner>, keys: &[String]) {
    let rows = match inner.storage.list_downloads().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not list the download rows");
            return;
        }
    };
    let left = inner
        .storage
        .local_variants(keys.to_vec())
        .await
        .unwrap_or_default();
    for row in rows {
        let song = keys
            .iter()
            .any(|key| *key == row.id || *key == row.track_id);
        let has_audio = left.contains_key(&row.track_id) || left.contains_key(&row.id);
        if !song || row.status != DownloadStatus::Done || has_audio {
            continue;
        }
        match inner.storage.delete_download(&row.id).await {
            Ok(true) => emit(&inner.events, DownloadEvent::Removed(row.id)),
            Ok(false) => {}
            Err(error) => tracing::warn!(id = %row.id, %error, "could not delete download"),
        }
    }
}

/// Deletes temporary files a crashed download left in the media folder
/// (the `.tmp…` names of the `tempfile` crate), once they are old enough
/// that no running download can own them.
async fn delete_stale_temp_files(media_dir: &std::path::Path) {
    let Ok(mut entries) = tokio::fs::read_dir(media_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_name().to_string_lossy().starts_with(".tmp") {
            continue;
        }
        let old = entry
            .metadata()
            .await
            .ok()
            .filter(std::fs::Metadata::is_file)
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_TEMP_AGE);
        if old {
            let path = entry.path();
            match tokio::fs::remove_file(&path).await {
                Ok(()) => tracing::info!(file = %path.display(), "deleted a partial download"),
                Err(error) => {
                    tracing::warn!(file = %path.display(), %error, "could not delete a partial download");
                }
            }
        }
    }
}

async fn handle_pause(inner: &Arc<Inner>, id: &str) {
    let record = match inner.storage.get_download(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%id, %error, "could not read download row");
            return;
        }
    };
    // Only a live row can be paused; Done / Failed / already-Paused stay put.
    if !matches!(
        record.status,
        DownloadStatus::Queued | DownloadStatus::Downloading
    ) {
        return;
    }
    // Abort the in-flight fetch (if running) and stop a deferred row from being
    // drained. The worker's cancelled branch leaves the row for us to set here.
    cancel_active(inner, id);
    if let Ok(mut set) = inner.pending_deferred.lock() {
        set.remove(id);
    }
    // Keep the last progress so the UI ring shows where it stopped.
    if let Err(error) = inner
        .storage
        .update_download_status(id, DownloadStatus::Paused, record.progress, None)
        .await
    {
        tracing::warn!(%id, %error, "could not pause download");
        return;
    }
    emit_status(inner, id, DownloadStatus::Paused, record.progress, None);
}

async fn handle_resume(inner: &Arc<Inner>, id: &str) {
    let record = match inner.storage.get_download(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%id, %error, "could not read download row");
            return;
        }
    };
    if record.status != DownloadStatus::Paused {
        return;
    }
    // Re-fetch from scratch: true byte-resume would need downloader range
    // support, so progress restarts at zero.
    if let Err(error) = inner
        .storage
        .update_download_status(id, DownloadStatus::Queued, 0.0, None)
        .await
    {
        tracing::warn!(%id, %error, "could not resume download");
        return;
    }
    emit_status(inner, id, DownloadStatus::Queued, 0.0, None);
    let mut next = record;
    next.status = DownloadStatus::Queued;
    next.progress = 0.0;
    next.message = None;
    dispatch(inner, next);
}

async fn handle_clear_finished(inner: &Arc<Inner>) {
    match inner
        .storage
        .delete_downloads_by_status(DownloadStatus::Done)
        .await
    {
        Ok(ids) => {
            for id in ids {
                emit(&inner.events, DownloadEvent::Removed(id));
            }
        }
        Err(error) => {
            tracing::warn!(%error, "could not clear finished downloads");
        }
    }
}

/// See [`DownloadQueue::remove_downloads`].
async fn handle_remove_downloads(
    inner: &Arc<Inner>,
    owner: Option<&str>,
    track_ids: Vec<String>,
) -> Vec<String> {
    let orphans = if let Some(owner) = owner {
        if let Err(error) = inner.storage.unregister_download_collection(owner).await {
            tracing::warn!(%owner, %error, "could not forget the collection");
        }
        // Every pin of the collection goes, also those its current songs no
        // longer name (a song that left it upstream).
        match inner
            .storage
            .release_download_collection(owner, track_ids)
            .await
        {
            Ok(orphans) => orphans,
            Err(error) => {
                tracing::warn!(%owner, %error, "could not unpin collection download");
                return Vec::new();
            }
        }
    } else {
        for id in &track_ids {
            if let Err(error) = inner.storage.unpin_track(id).await {
                tracing::warn!(%id, %error, "could not unpin download");
            }
        }
        track_ids
    };
    let rows = inner
        .storage
        .list_downloads()
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "could not list the download rows");
            Vec::new()
        });
    let mut removed = Vec::with_capacity(orphans.len());
    for id in orphans {
        // Custom songs have no pins, so they always come back as orphans:
        // their audio, and songs they play by reference, are never removed.
        if removal_exempt(inner, &id).await {
            continue;
        }
        // Every queue row of the song goes, finished ones included, so no
        // removed song lingers in the Downloads panel.
        handle_cancel(inner, &id).await;
        for row in rows_of_song(&rows, &id) {
            handle_cancel(inner, row).await;
        }
        if delete_local_audio(inner, &id).await {
            removed.push(id);
        }
    }
    if !removed.is_empty() {
        tracing::info!(
            owner = owner.unwrap_or("songs"),
            count = removed.len(),
            "downloads removed"
        );
        emit(&inner.events, DownloadEvent::AudioRemoved(removed.clone()));
    }
    removed
}

/// Deletes the downloaded audio of download key `id` (files inside the media
/// directory only) and clears it from the track rows. Returns whether any
/// audio was recorded or found.
async fn delete_local_audio(inner: &Arc<Inner>, id: &str) -> bool {
    if removal_exempt(inner, id).await {
        return false;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(Some(record)) = inner.storage.find_track(id).await {
        files.extend(
            record
                .track
                .audio
                .iter()
                .map(|(_, path)| path.to_path_buf()),
        );
    }
    if crate::media::is_valid_media_id(id) {
        for ext in AUDIO_EXTENSIONS {
            files.push(inner.media_dir.join(format!("{id}.{ext}")));
        }
        files.push(
            inner
                .media_dir
                .join(crate::download::media_file_name(id, AudioVariant::Spatial)),
        );
    }
    let mut found = false;
    for path in files {
        if !path.starts_with(&inner.media_dir) {
            continue;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => found = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not delete audio file");
            }
        }
    }
    // `changed` counts audio rows actually deleted, so a song without
    // downloaded audio is not reported as removed.
    match inner.storage.clear_audio_by_any_id(id).await {
        Ok(changed) => found || changed > 0,
        Err(error) => {
            tracing::warn!(%id, %error, "could not clear audio path");
            found
        }
    }
}

/// Whether removing downloads must leave `id`'s audio alone (a custom song,
/// or a song a custom song plays by reference). A lookup error counts as
/// exempt: keeping a file is the safe side.
async fn removal_exempt(inner: &Arc<Inner>, id: &str) -> bool {
    match inner.storage.audio_removal_exempt(id).await {
        Ok(exempt) => exempt,
        Err(error) => {
            tracing::warn!(%id, %error, "could not check whether audio may be removed");
            true
        }
    }
}

/// How one fetch attempt ended.
enum FetchOutcome {
    /// The worker's token fired (pause / cancel); the actor owns the row state.
    Cancelled,
    /// Every planned version is on disk and recorded (`files` holds the ones
    /// this attempt wrote), fetched for `target`. `skipped`: some mix was
    /// not fetched because it was already on disk.
    Done {
        files: Vec<Downloaded>,
        target: Target,
        skipped: bool,
    },
    /// The track can never be downloaded.
    Unavailable(String),
    /// The attempt failed; `retryable` says whether trying again may help and
    /// `retry_after` is Apple's minimum wait, when it sent one.
    Failed {
        message: String,
        retryable: bool,
        retry_after: Option<Duration>,
    },
}

impl FetchOutcome {
    fn failed(message: String, retryable: bool) -> Self {
        Self::Failed {
            message,
            retryable,
            retry_after: None,
        }
    }
}

/// Maps a downloader error to what the queue does next (see the table on
/// [`DownloadError`]).
fn classify(error: DownloadError) -> FetchOutcome {
    match error {
        DownloadError::Unavailable { reason, .. } => FetchOutcome::Unavailable(reason),
        DownloadError::Transient {
            message,
            status,
            retry_after,
            ..
        } => FetchOutcome::Failed {
            message: match status {
                Some(status) if !message.contains(&status.to_string()) => {
                    format!("{message} (HTTP {status})")
                }
                _ => message,
            },
            retryable: true,
            retry_after,
        },
        DownloadError::AuthExpired { message, .. } => {
            FetchOutcome::failed(format!("{AUTH_EXPIRED_HINT} ({message})"), false)
        }
        DownloadError::Cancelled => FetchOutcome::Cancelled,
        // The queue handles a missing variant before classifying; reaching
        // here means a downloader returned it for a variant it was not asked
        // to fall back from.
        DownloadError::VariantUnavailable { .. } => FetchOutcome::Unavailable(error.to_string()),
        DownloadError::SourceRequired
        | DownloadError::UnsupportedFormat(_)
        | DownloadError::InvalidTrackId(_) => FetchOutcome::failed(error.to_string(), false),
        DownloadError::Io(_) => FetchOutcome::failed(error.to_string(), true),
        DownloadError::Other(message) => FetchOutcome::failed(message, true),
    }
}

/// Delay before retry number `attempt` (2 = first retry) from `base`
/// ([`BACKOFF_BASE`] outside tests): exponential with
/// ±25% jitter, capped at [`BACKOFF_MAX`].
fn backoff_delay(base: Duration, attempt: u32, id: &str) -> Duration {
    let exponent = attempt.saturating_sub(2);
    let base = base
        .saturating_mul(BACKOFF_FACTOR.saturating_pow(exponent))
        .min(BACKOFF_MAX);
    // Random per call (RandomState is randomly keyed); hashing the id and
    // attempt spreads simultaneous retries apart.
    let noise = RandomState::new().hash_one((id, attempt)) % 1_000;
    let millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    // 750..1250 per mille of the base delay.
    Duration::from_millis(millis.saturating_mul(750 + noise / 2) / 1_000)
}

/// Waits until the previous worker of the same row has exited (aborting it
/// if it ignores its cancellation for too long).
async fn wait_for_previous(inner: &Inner, id: &str, previous: Option<JoinHandle<()>>) {
    let Some(mut previous) = previous else {
        return;
    };
    let grace = inner.timing.cancel_grace.saturating_mul(2);
    if tokio::time::timeout(grace, &mut previous).await.is_err() {
        tracing::warn!(%id, "previous download worker did not stop; aborting it");
        previous.abort();
        let _ = previous.await;
    }
}

/// The task behind one row: waits for the row's previous worker, then runs
/// attempts (with retries) until the row is finished, paused or cancelled.
async fn run_worker(
    inner: Arc<Inner>,
    record: DownloadRecord,
    token: CancellationToken,
    epoch: u64,
    previous: Option<JoinHandle<()>>,
    flags: WorkerFlags,
) {
    wait_for_previous(&inner, &record.id, previous).await;
    let worker = WorkerRef {
        record: &record,
        token: &token,
        epoch,
        flags: &flags,
    };
    let mut attempt = 1u32;
    // Versions already written by an earlier attempt of this worker, so a
    // retry after the second version failed does not fetch the first again.
    let mut done: Vec<AudioVariant> = Vec::new();
    while !token.is_cancelled() {
        match run_attempt(&inner, &worker, attempt, &mut done).await {
            AttemptEnd::Finished => break,
            AttemptEnd::Again => {
                tracing::info!(id = %record.id, "re-download merged in; fetching the skipped mixes");
                attempt = 1;
            }
            AttemptEnd::Retry {
                message,
                retry_after,
            } => {
                attempt += 1;
                let delay = backoff_delay(inner.timing.backoff_base, attempt, &record.id)
                    .max(retry_after.unwrap_or_default().min(RETRY_AFTER_MAX));
                let note = format!(
                    "Retrying in {} s (attempt {attempt} of {MAX_ATTEMPTS}): {message}",
                    delay.as_secs().max(1)
                );
                tracing::info!(id = %record.id, attempt, delay_ms = delay.as_millis(), "download retry scheduled");
                worker_status(
                    &inner,
                    &record.id,
                    epoch,
                    DownloadStatus::Queued,
                    0.0,
                    None,
                    attempt,
                    Some(note),
                )
                .await;
                emit_summary(&inner).await;
                tokio::select! {
                    biased;
                    () = token.cancelled() => break,
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    clear_active(&inner, &record.id, epoch);
    emit_summary(&inner).await;
}

/// Whether a re-download merged into this worker still has to replace the
/// mixes it skipped as already on disk (consumes the request).
fn take_merged_redownload(skipped: bool, flags: &WorkerFlags) -> bool {
    skipped && flags.upgraded.swap(false, Ordering::SeqCst)
}

/// How an attempt ends once its row is `Done`: again when a re-download
/// merged in meanwhile, so the skipped mixes are replaced after all.
fn after_finish(skipped: bool, flags: &WorkerFlags) -> AttemptEnd {
    if take_merged_redownload(skipped, flags) {
        AttemptEnd::Again
    } else {
        AttemptEnd::Finished
    }
}

/// What a worker's steps need to know about it.
struct WorkerRef<'a> {
    record: &'a DownloadRecord,
    token: &'a CancellationToken,
    /// Generation its row writes are guarded by.
    epoch: u64,
    flags: &'a WorkerFlags,
}

/// How one attempt of a worker ended.
enum AttemptEnd {
    /// Nothing more to do (done, failed for good, unavailable, cancelled).
    Finished,
    /// A re-download was merged in after mixes were skipped as already on
    /// disk: run again, replacing them.
    Again,
    /// A temporary failure; retry after a backoff of at least `retry_after`.
    Retry {
        message: String,
        retry_after: Option<Duration>,
    },
}

async fn run_attempt(
    inner: &Arc<Inner>,
    worker: &WorkerRef<'_>,
    attempt: u32,
    done: &mut Vec<AudioVariant>,
) -> AttemptEnd {
    let record = worker.record;
    let token = worker.token;
    // Wait for a slot — but abort at once if a pause/cancel arrives.
    let permit = tokio::select! {
        biased;
        () = token.cancelled() => return AttemptEnd::Finished,
        permit = inner.gate.acquire() => permit,
    };

    // If playback restarted while we waited, defer this row again instead of
    // grabbing a slot that a manual download could use.
    if record.deferred && !inner.is_idle() {
        if let Ok(mut set) = inner.pending_deferred.lock() {
            set.insert(record.id.clone());
        }
        drop(permit);
        return AttemptEnd::Finished;
    }

    // Space fetch starts apart (each one begins with playback + license
    // requests to Apple).
    let start_at = inner.reserve_start();
    tokio::select! {
        biased;
        () = token.cancelled() => return AttemptEnd::Finished,
        () = tokio::time::sleep_until(start_at) => {}
    }

    let (start, _) = DownloadStage::Requesting.band();
    if !worker_status(
        inner,
        &record.id,
        worker.epoch,
        DownloadStatus::Downloading,
        start,
        Some(DownloadStage::Requesting),
        attempt,
        None,
    )
    .await
    {
        // Paused or cancelled meanwhile.
        return AttemptEnd::Finished;
    }
    emit_summary(inner).await;

    let Some(downloader) = inner.downloaders.get(DOWNLOADER_ID) else {
        let message = Some(format!(
            "No {DOWNLOADER_ID:?} downloader is registered; drop a device.wvd file in the data \
             folder and restart."
        ));
        finish_with(inner, worker, DownloadStatus::Failed, message).await;
        return AttemptEnd::Finished;
    };

    let started = Instant::now();
    let outcome = run_fetch(inner, worker, attempt, downloader.as_ref(), done).await;
    drop(permit);
    match outcome {
        FetchOutcome::Cancelled => {
            tracing::debug!(id = %record.id, "download worker stopped by pause/cancel");
            AttemptEnd::Finished
        }
        FetchOutcome::Done {
            files,
            target,
            skipped,
        } => {
            if !files.is_empty() {
                let per_file = u32::try_from(files.len()).unwrap_or(1);
                inner.record_fetch_time(started.elapsed() / per_file);
            }
            if take_merged_redownload(skipped, worker.flags) {
                return AttemptEnd::Again;
            }
            finish_done(inner, worker, &files, &target).await;
            // A re-download can merge in while the row is being finished;
            // it still has to replace the mixes this attempt skipped.
            after_finish(skipped, worker.flags)
        }
        FetchOutcome::Unavailable(reason) => {
            tracing::info!(id = %record.id, %reason, "download not supported yet");
            finish_with(inner, worker, DownloadStatus::Unavailable, Some(reason)).await;
            AttemptEnd::Finished
        }
        FetchOutcome::Failed {
            message,
            retryable,
            retry_after,
        } => {
            tracing::warn!(id = %record.id, attempt, retryable, message, "download failed");
            if retryable {
                inner.cool_down(retry_after);
            }
            if retryable && attempt < MAX_ATTEMPTS {
                AttemptEnd::Retry {
                    message,
                    retry_after,
                }
            } else {
                finish_with(inner, worker, DownloadStatus::Failed, Some(message)).await;
                AttemptEnd::Finished
            }
        }
    }
}

/// Live progress state of one fetch.
struct ProgressState {
    started: Instant,
    estimate: Duration,
    /// Part of the row's ring this fetch fills (`(0.0, 1.0)` unless the row
    /// fetches several versions one after the other).
    span: (f32, f32),
    /// Stage reported by the downloader, if it reports any.
    reported: Option<DownloadStage>,
    /// Byte fraction of the fetch stage, when the downloader reports bytes.
    bytes: Option<f32>,
    /// Last published fraction and stage.
    shown: f32,
    shown_stage: Option<DownloadStage>,
    persisted: f32,
}

impl ProgressState {
    #[cfg(test)]
    fn new(estimate: Duration) -> Self {
        Self::with_span(estimate, (0.0, 1.0))
    }

    /// A fetch that fills `span` of the ring.
    fn with_span(estimate: Duration, span: (f32, f32)) -> Self {
        let (start, _) = DownloadStage::Requesting.band();
        let start = span.0 + (span.1 - span.0) * start;
        Self {
            started: Instant::now(),
            estimate,
            span,
            reported: None,
            bytes: None,
            shown: start,
            shown_stage: Some(DownloadStage::Requesting),
            persisted: start,
        }
    }

    /// Applies one report from the downloader. Any report switches the stage
    /// from the time-based estimate to what the downloader says.
    fn apply(&mut self, report: ProgressReport) {
        match report {
            ProgressReport::Stage(phase) => self.reported = Some(phase),
            ProgressReport::Bytes { done, total } => {
                if let Some(total) = total.filter(|t| *t > 0) {
                    self.bytes = Some(progress_fraction(done, total));
                    self.reported.get_or_insert(DownloadStage::Fetching);
                }
            }
        }
    }

    /// Current `(stage, fraction)`: reported stages and byte counts when the
    /// downloader provides them, otherwise an estimate from elapsed time.
    fn current(&self) -> (DownloadStage, f32) {
        let elapsed = self.started.elapsed().as_secs_f32();
        let estimate = self.estimate.as_secs_f32().max(1.0);
        let stage = self.reported.unwrap_or(
            // The playback info and the license take the first fraction of a
            // fetch; the rest is segment download + decryption.
            if elapsed < estimate * 0.06 {
                DownloadStage::Requesting
            } else if elapsed < estimate * 0.12 {
                DownloadStage::License
            } else {
                DownloadStage::Fetching
            },
        );
        let (from, to) = stage.band();
        let within = match (stage, self.bytes) {
            (DownloadStage::Fetching, Some(fraction)) => fraction,
            // Approaches (never reaches) the end of the band: 1 - e^(-2t/T).
            _ => 1.0 - (-2.0 * elapsed / estimate).exp(),
        };
        let fraction = from + (to - from) * within.clamp(0.0, 0.97);
        let (lo, hi) = self.span;
        (stage, (lo + (hi - lo) * fraction).max(self.shown))
    }
}

/// One version a row fetches, and what to do when the song lacks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    variant: AudioVariant,
    /// When the song has no such version: fetch the normal one instead
    /// (`true`), or skip it (`false`).
    fall_back_to_stereo: bool,
}

impl Step {
    const STEREO: Self = Self {
        variant: AudioVariant::Stereo,
        fall_back_to_stereo: false,
    };
}

/// The versions to fetch for `format`, given whether the downloader
/// supports spatial and whether the song has a spatial master playlist.
fn plan_steps(format: DownloadFormat, spatial_master: bool) -> Vec<Step> {
    let spatial = |fall_back_to_stereo| Step {
        variant: AudioVariant::Spatial,
        fall_back_to_stereo,
    };
    match (format, spatial_master) {
        (DownloadFormat::Stereo, _) | (_, false) => vec![Step::STEREO],
        (DownloadFormat::SpatialWhereAvailable, true) => vec![spatial(true)],
        (DownloadFormat::Both, true) => vec![Step::STEREO, spatial(false)],
    }
}

/// The part of a row's ring one fetch fills: the next of `total` versions
/// after `finished` written ones. `total` counts the versions still to go
/// as planned now, so a fallback (spatial missing, stereo instead) takes the
/// skipped version's place instead of running past the end of the ring.
fn step_span(finished: usize, total: usize) -> (f32, f32) {
    let total = total.max(finished + 1);
    #[expect(
        clippy::cast_precision_loss,
        reason = "at most two versions per row; display-only fraction"
    )]
    let fraction = |n: usize| n as f32 / total as f32;
    (fraction(finished), fraction(finished + 1))
}

/// Where the queue looks up a song's spatial stream: only for a song, a
/// format that wants it and a downloader that supports it.
async fn spatial_master_for(
    inner: &Inner,
    downloader: &dyn crate::download::Downloader,
    target: &Target,
) -> Result<Option<String>, DownloadError> {
    let format = inner.download_format();
    if format == DownloadFormat::Stereo
        || target.kind != MediaKind::Song
        || !downloader.capabilities().spatial
    {
        return Ok(None);
    }
    download_target::spatial_master(inner.spatial.as_ref(), &target.catalog_id).await
}

/// The mixes of `target` (or of the queued id) already recorded with a file
/// that exists on disk.
async fn present_variants(
    inner: &Inner,
    record: &DownloadRecord,
    target: &Target,
) -> Vec<AudioVariant> {
    let mut present = Vec::new();
    for id in [target.catalog_id.as_str(), record.track_id.as_str()] {
        let files = match inner.storage.track_audio_files(id).await {
            Ok(files) => files,
            Err(error) => {
                tracing::warn!(%id, %error, "could not read the recorded audio files");
                continue;
            }
        };
        for file in files {
            if !present.contains(&file.variant)
                && tokio::fs::try_exists(&file.path).await.unwrap_or(false)
            {
                present.push(file.variant);
            }
        }
    }
    present
}

/// Resolves what the row fetches (the catalog id, request shape, media
/// kind and library twin); a music video the downloader cannot handle is
/// unavailable.
async fn resolve_target(
    inner: &Inner,
    worker: &WorkerRef<'_>,
    downloader: &dyn crate::download::Downloader,
) -> Result<Target, Box<FetchOutcome>> {
    let record = worker.record;
    let resolved = tokio::select! {
        biased;
        () = worker.token.cancelled() => return Err(Box::new(FetchOutcome::Cancelled)),
        resolved = download_target::resolve(&inner.storage, inner.search.as_ref(), &record.track_id) => resolved,
    };
    let target = resolved.map_err(|error| Box::new(classify(error)))?;
    if target.kind == MediaKind::MusicVideo && !downloader.capabilities().music_videos {
        return Err(Box::new(FetchOutcome::Unavailable(
            MUSIC_VIDEO_REASON.to_owned(),
        )));
    }
    tracing::debug!(id = %record.id, ?target, "download target resolved");
    Ok(target)
}

/// Whether `present` already holds every mix `format` could want, so a
/// plain download has nothing to ask Apple for (not even whether the song
/// has a spatial mix).
/// `known` is whether the song has a spatial mix, when that was learned
/// before; a downloader without spatial support only ever needs stereo.
fn all_wanted_present(
    format: DownloadFormat,
    spatial: bool,
    present: &[AudioVariant],
    known: Option<bool>,
) -> bool {
    if present.is_empty() {
        return false;
    }
    let format = if spatial {
        format
    } else {
        DownloadFormat::Stereo
    };
    !crate::download_needs::needs_download(format, present, known)
}

/// Whether the song was looked up for its spatial mix before.
async fn known_spatial(inner: &Inner, key: &str) -> Option<bool> {
    match inner.storage.song_spatial().await {
        Ok(known) => known.get(key).copied(),
        Err(error) => {
            tracing::warn!(%key, %error, "could not read the known spatial mixes");
            None
        }
    }
}

/// Whether [`spatial_master_for`] asked the catalog (and its answer says
/// whether the song has a spatial mix).
fn spatial_looked_up(
    format: DownloadFormat,
    downloader: &dyn crate::download::Downloader,
    target: &Target,
) -> bool {
    format != DownloadFormat::Stereo
        && target.kind == MediaKind::Song
        && downloader.capabilities().spatial
}

/// Remembers whether the song has a spatial mix, so "Download missing songs"
/// knows whether a stereo-only song is complete.
async fn learn_spatial(inner: &Inner, key: &str, has_spatial: bool) {
    if let Err(error) = inner.storage.set_song_spatial(key, has_spatial).await {
        tracing::warn!(%key, %error, "could not record whether the song has a spatial mix");
    }
}

/// Resolves what to fetch, plans the versions per the download format,
/// fetches each one (recording every finished file at once) and returns the
/// outcome. Versions in `done` were written by an earlier attempt and are
/// skipped; versions written now are added to it. Unless the row is a
/// re-download, mixes already on disk are skipped too (and `skipped` says
/// so).
async fn run_fetch(
    inner: &Arc<Inner>,
    worker: &WorkerRef<'_>,
    attempt: u32,
    downloader: &dyn crate::download::Downloader,
    done: &mut Vec<AudioVariant>,
) -> FetchOutcome {
    let record = worker.record;
    let token = worker.token;
    let target = match resolve_target(inner, worker, downloader).await {
        Ok(target) => target,
        Err(outcome) => return *outcome,
    };
    let format = inner.download_format();
    let replace = worker.flags.replace.load(Ordering::SeqCst);
    let present = if replace {
        Vec::new()
    } else {
        present_variants(inner, record, &target).await
    };
    let known = known_spatial(inner, &target.catalog_id).await;
    if all_wanted_present(format, downloader.capabilities().spatial, &present, known) {
        tracing::debug!(id = %record.id, ?present, "every wanted mix is already downloaded");
        return FetchOutcome::Done {
            files: Vec::new(),
            target,
            skipped: true,
        };
    }

    let master = tokio::select! {
        biased;
        () = token.cancelled() => return FetchOutcome::Cancelled,
        master = spatial_master_for(inner, downloader, &target) => master,
    };
    let master = match master {
        Ok(master) => master,
        Err(error) => return classify(error),
    };
    if spatial_looked_up(format, downloader, &target) {
        learn_spatial(inner, &target.catalog_id, master.is_some()).await;
    }
    let mut steps = plan_steps(format, master.is_some());
    let planned = steps.len();
    steps.retain(|step| !done.contains(&step.variant));
    // Versions an earlier attempt already wrote keep their share of the ring.
    let written_before = planned - steps.len();
    let before_present = steps.len();
    steps.retain(|step| !present.contains(&step.variant));
    let mut skipped = steps.len() < before_present;
    tracing::debug!(id = %record.id, ?steps, replace, "download versions planned");

    let mut files = Vec::with_capacity(steps.len());
    let mut index = 0usize;
    while index < steps.len() {
        let step = steps[index];
        if index > 0 {
            // A further version is a fetch of its own: space its license
            // requests like any other fetch start.
            let start_at = inner.reserve_start();
            tokio::select! {
                biased;
                () = token.cancelled() => return FetchOutcome::Cancelled,
                () = tokio::time::sleep_until(start_at) => {}
            }
        }
        let finished = written_before + files.len();
        let span = step_span(finished, finished + steps.len() - index);
        let request = DownloadRequest::new(target.catalog_id.clone(), inner.media_dir.clone())
            .with_variant(step.variant)
            .with_playback(target.playback.clone())
            .with_kind(target.kind)
            .with_enhanced_hls(master.clone())
            .with_info(record_info(record));
        match fetch_one(inner, worker, attempt, downloader, request, span).await {
            Ok(file) => {
                // A file that completed is kept and recorded, even when the
                // row was paused or cancelled while it was being saved.
                record_file(inner, record, &target, &file).await;
                done.push(step.variant);
                if file.variant != step.variant {
                    done.push(file.variant);
                }
                files.push(file);
                if token.is_cancelled() {
                    return FetchOutcome::Cancelled;
                }
            }
            Err(outcome) => match *outcome {
                FetchOutcome::Unavailable(reason) if step.variant == AudioVariant::Spatial => {
                    tracing::info!(id = %record.id, %reason, "no spatial version");
                    learn_spatial(inner, &target.catalog_id, false).await;
                    if step.fall_back_to_stereo && !done.contains(&AudioVariant::Stereo) {
                        if present.contains(&AudioVariant::Stereo) {
                            skipped = true;
                        } else {
                            steps.push(Step::STEREO);
                        }
                    }
                }
                outcome => return outcome,
            },
        }
        index += 1;
    }
    FetchOutcome::Done {
        files,
        target,
        skipped,
    }
}

/// Drives one [`crate::download::Downloader::fetch`] while publishing staged
/// progress (filling `span` of the ring) and watching the worker's token.
/// When the token fires, the fetch gets [`Timing::cancel_grace`] to notice
/// (the downloader checks its own child token) and stop, so a new fetch of
/// the same song can never start while this one still runs. A fetch that
/// still completes is returned as a success: its file is whole.
async fn fetch_one(
    inner: &Arc<Inner>,
    worker: &WorkerRef<'_>,
    attempt: u32,
    downloader: &dyn crate::download::Downloader,
    request: DownloadRequest,
    span: (f32, f32),
) -> Result<Downloaded, Box<FetchOutcome>> {
    let record = worker.record;
    let token = worker.token;
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressReport>();
    let request = request
        .with_progress(DownloadProgress::new(progress_tx))
        .with_cancel(token.child_token());
    let variant = request.variant;

    let fetch = downloader.fetch(&request);
    tokio::pin!(fetch);
    let mut state = ProgressState::with_span(inner.fetch_estimate(), span);
    let mut tick = tokio::time::interval(PROGRESS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            biased;
            () = token.cancelled() => {
                match tokio::time::timeout(inner.timing.cancel_grace, &mut fetch).await {
                    Ok(Ok(file)) => break Ok(file),
                    Ok(Err(_)) => {}
                    Err(_) => {
                        tracing::warn!(id = %record.id, "the downloader did not stop after cancel; dropping it");
                    }
                }
                return Err(Box::new(FetchOutcome::Cancelled));
            }
            Some(report) = progress_rx.recv() => {
                tracing::debug!(id = %record.id, ?report, "downloader progress report");
                state.apply(report);
                publish_progress(inner, worker, attempt, &mut state).await;
            }
            _ = tick.tick() => publish_progress(inner, worker, attempt, &mut state).await,
            result = &mut fetch => break result,
        }
    };
    match result {
        Ok(file) => Ok(file),
        // Only our token may cancel a fetch; anything else is a
        // downloader bug, so retry instead of leaving the row running.
        Err(DownloadError::Cancelled) if !token.is_cancelled() => {
            Err(Box::new(FetchOutcome::failed(
                "the downloader stopped without being cancelled".to_owned(),
                true,
            )))
        }
        Err(DownloadError::Cancelled) => Err(Box::new(FetchOutcome::Cancelled)),
        Err(error @ DownloadError::VariantUnavailable { .. }) => Err(Box::new(
            FetchOutcome::Unavailable(format!("{error} ({variant})")),
        )),
        Err(error) => Err(Box::new(classify(error))),
    }
}

/// Records a written file (with its size and duration) as the audio of its
/// variant on every track row with the fetched catalog id, and of the queued
/// id when that differs.
async fn record_file(
    inner: &Inner,
    record: &DownloadRecord,
    target: &Target,
    downloaded: &Downloaded,
) {
    let size = match tokio::fs::metadata(&downloaded.path).await {
        Ok(meta) => Some(meta.len()),
        Err(error) => {
            tracing::warn!(path = %downloaded.path.display(), %error, "could not read the file size");
            None
        }
    };
    let file = AudioFile {
        variant: downloaded.variant,
        path: downloaded.path.clone(),
        size,
        duration_ms: downloaded.duration_ms,
    };
    tracing::info!(
        id = %record.id,
        variant = %downloaded.variant,
        path = %downloaded.path.display(),
        size = ?size,
        duration_ms = ?downloaded.duration_ms,
        "download version written"
    );
    for any_id in [target.catalog_id.as_str(), record.track_id.as_str()] {
        if let Err(error) = inner
            .storage
            .put_audio_file_by_any_id(any_id, file.clone())
            .await
        {
            tracing::warn!(id = %record.id, %error, "could not record the audio file");
        }
        if record.track_id == target.catalog_id {
            break;
        }
    }
}

/// Publishes (and occasionally persists) the current staged progress.
async fn publish_progress(
    inner: &Arc<Inner>,
    worker: &WorkerRef<'_>,
    attempt: u32,
    state: &mut ProgressState,
) {
    let id = worker.record.id.as_str();
    let (phase, fraction) = state.current();
    let phase_changed = state.shown_stage != Some(phase);
    if !phase_changed && fraction < state.shown + PROGRESS_STEP {
        return;
    }
    state.shown = fraction;
    state.shown_stage = Some(phase);
    tracing::debug!(%id, phase = phase.label(), progress = fraction, "download progress");
    if phase_changed || fraction >= state.persisted + PERSIST_STEP {
        state.persisted = fraction;
        worker_status(
            inner,
            id,
            worker.epoch,
            DownloadStatus::Downloading,
            fraction,
            Some(phase),
            attempt,
            None,
        )
        .await;
    } else {
        emit_if_current(
            inner,
            id,
            worker.epoch,
            DownloadStatus::Downloading,
            fraction,
            Some(phase),
            attempt,
            None,
        );
    }
}

fn record_info(record: &DownloadRecord) -> TrackInfo {
    TrackInfo {
        title: record.title.clone(),
        artist: record.artist.clone(),
        album: record.album.clone(),
        duration_ms: (record.duration_ms != 0).then_some(record.duration_ms),
    }
}

/// Finishes a download whose files [`record_file`] already recorded: a
/// library-only item is linked to the twin it was fetched as, the row turns
/// `Done`, and lyrics are fetched.
async fn finish_done(
    inner: &Arc<Inner>,
    worker: &WorkerRef<'_>,
    files: &[Downloaded],
    target: &Target,
) {
    let record = worker.record;
    let id = record.id.as_str();
    let variants: Vec<AudioVariant> = files.iter().map(|file| file.variant).collect();
    tracing::info!(%id, ?variants, "download finished");
    emit_if_current(
        inner,
        id,
        worker.epoch,
        DownloadStatus::Downloading,
        DownloadStage::Saving.band().0,
        Some(DownloadStage::Saving),
        1,
        None,
    );
    if let Some(original) = &target.twin_of {
        match inner
            .storage
            .link_catalog_twin(original, &target.catalog_id)
            .await
        {
            Ok(_) => {
                tracing::info!(item = %original, twin = %target.catalog_id, "library item linked to its twin");
            }
            Err(error) => {
                tracing::warn!(%id, %error, "could not link the library item to its twin");
            }
        }
        if let Err(error) = inner
            .storage
            .pin_downloads(pin_owner(record), vec![target.catalog_id.clone()])
            .await
        {
            tracing::warn!(%id, %error, "could not pin the twin download");
        }
    }
    finish_with(inner, worker, DownloadStatus::Done, None).await;

    spawn_lyrics_fetch(inner, record, &target.catalog_id);
}

/// Fetches lyrics for a just-downloaded track on its own task, so the lookup
/// neither holds a download slot nor affects the (already `Done`) row.
fn spawn_lyrics_fetch(inner: &Arc<Inner>, record: &DownloadRecord, catalog_id: &str) {
    let Some(fetcher) = inner.lyrics.clone() else {
        return;
    };
    let track_id = catalog_id.to_owned();
    let info = record_info(record);
    tokio::spawn(async move {
        match fetcher.fetch_lyrics(&track_id, &info).await {
            Ok(None) => tracing::debug!(track = %track_id, "lyrics already present"),
            Ok(Some(source)) => {
                tracing::debug!(track = %track_id, ?source, "post-download lyrics fetched");
            }
            Err(error @ LibraryError::LyricsDeferred { .. }) => {
                tracing::info!(track = %track_id, %error, "post-download lyrics deferred");
            }
            Err(error) => {
                tracing::warn!(track = %track_id, %error, "post-download lyrics fetch failed");
            }
        }
    });
}

/// Progress fraction in `0.0..=1.0` from byte counts.
#[expect(
    clippy::cast_precision_loss,
    reason = "progress is display-only; sub-ULP error in the ratio is irrelevant"
)]
fn progress_fraction(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (done as f32 / total as f32).clamp(0.0, 1.0)
}

/// Persists and publishes a worker's terminal status (`Done`, `Failed`,
/// `Unavailable`), unless the row was paused or cancelled meanwhile.
async fn finish_with(
    inner: &Arc<Inner>,
    worker: &WorkerRef<'_>,
    status: DownloadStatus,
    message: Option<String>,
) {
    let progress = if status == DownloadStatus::Done {
        1.0
    } else {
        0.0
    };
    worker_status(
        inner,
        &worker.record.id,
        worker.epoch,
        status,
        progress,
        None,
        1,
        message,
    )
    .await;
}

fn emit_status(
    inner: &Arc<Inner>,
    id: &str,
    status: DownloadStatus,
    progress: f32,
    message: Option<String>,
) {
    emit_live(inner, id, status, progress, None, 1, message);
}

fn emit_live(
    inner: &Arc<Inner>,
    id: &str,
    status: DownloadStatus,
    progress: f32,
    stage: Option<DownloadStage>,
    attempt: u32,
    message: Option<String>,
) {
    emit(
        &inner.events,
        DownloadEvent::StatusChanged {
            id: id.to_owned(),
            status,
            progress,
            message,
            stage,
            attempt,
        },
    );
    // Mirror onto the minimal progress stream. No receivers is a normal
    // state; the send error is ignored.
    let _ = inner.progress_events.send(DownloadProgressUpdate {
        id: id.to_owned(),
        status,
        progress,
        failed: status == DownloadStatus::Failed,
        stage,
    });
}

async fn emit_summary(inner: &Arc<Inner>) {
    match inner.storage.list_downloads().await {
        Ok(rows) => {
            let mut active = 0;
            let mut queued = 0;
            let mut done = 0;
            let mut failed = 0;
            let mut unavailable = 0;
            let mut downloading = false;
            for row in rows {
                downloading |= row.status == DownloadStatus::Downloading;
                match row.status {
                    DownloadStatus::Downloading | DownloadStatus::Paused => active += 1,
                    DownloadStatus::Queued => queued += 1,
                    DownloadStatus::Done => done += 1,
                    DownloadStatus::Failed => failed += 1,
                    DownloadStatus::Unavailable => unavailable += 1,
                }
            }
            if let Some(lyrics) = &inner.lyrics {
                lyrics.downloads_idle(!downloading && queued == 0);
            }
            emit(
                &inner.events,
                DownloadEvent::SummaryChanged {
                    active,
                    queued,
                    done,
                    failed,
                    unavailable,
                },
            );
        }
        Err(error) => {
            tracing::warn!(%error, "could not compute download summary");
        }
    }
}

fn emit(tx: &broadcast::Sender<DownloadEvent>, event: DownloadEvent) {
    // No receivers is a normal state (the UI has not subscribed yet).
    let _ = tx.send(event);
}

/// Builds a flat list of [`DownloadRecord`]s from a resolved catalog item.
///
/// Album and playlist entries carry their parent id and name so the UI can
/// group rows; single songs leave those fields empty.
#[must_use]
pub fn records_from_catalog(item: &CatalogItem) -> Vec<DownloadRecord> {
    match item {
        CatalogItem::Album(album) => album
            .tracks
            .iter()
            .map(|song| {
                record_from_song(
                    song,
                    DownloadSourceKind::Album,
                    Some(&album.id),
                    Some(&album.name),
                )
            })
            .collect(),
        CatalogItem::Playlist(playlist) => playlist
            .tracks
            .iter()
            .map(|song| {
                record_from_song(
                    song,
                    DownloadSourceKind::Playlist,
                    Some(&playlist.id),
                    Some(&playlist.name),
                )
            })
            .collect(),
        CatalogItem::Song(song) => {
            vec![record_from_song(song, DownloadSourceKind::Song, None, None)]
        }
    }
}

fn record_from_song(
    song: &Song,
    kind: DownloadSourceKind,
    source_id: Option<&str>,
    source_name: Option<&str>,
) -> DownloadRecord {
    DownloadRecord {
        id: song.id.clone(),
        track_id: song.id.clone(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        duration_ms: song.duration_ms,
        artwork_url: song.artwork_url.clone(),
        source_kind: kind,
        source_id: source_id.map(str::to_owned),
        source_name: source_name.map(str::to_owned),
        status: DownloadStatus::Queued,
        progress: 0.0,
        message: None,
        deferred: false,
        replace: false,
        // `0` lets storage fill both timestamps on insert.
        created_at: 0,
        updated_at: 0,
    }
}

/// A queued [`DownloadRecord`] for a stored track, keyed on its download key
/// (the catalog id, which the DRM downloader needs, else the track id) and
/// tagged with `source` (`None` for a single song).
#[must_use]
pub fn record_from_track(
    track: &crate::storage::Track,
    source: Option<(DownloadSourceKind, &str, &str)>,
) -> DownloadRecord {
    let key = track.catalog_id.clone().unwrap_or_else(|| track.id.clone());
    let (source_kind, source_id, source_name) = match source {
        Some((kind, id, name)) => (
            kind,
            Some(id.to_owned()),
            (!name.is_empty()).then(|| name.to_owned()),
        ),
        None => (DownloadSourceKind::Song, None, None),
    };
    DownloadRecord {
        id: key.clone(),
        track_id: key,
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration_ms: track.duration_ms.unwrap_or(0),
        artwork_url: track.artwork_url.clone(),
        source_kind,
        source_id,
        source_name,
        status: DownloadStatus::Queued,
        progress: 0.0,
        message: None,
        deferred: false,
        replace: false,
        // `0` lets storage fill both timestamps on insert.
        created_at: 0,
        updated_at: 0,
    }
}

/// "Keep downloaded": queues the songs added to kept collections since they
/// were downloaded. Run after a library sync.
///
/// A member counts as new when the collection has no pin on it yet. Each new
/// member is pinned, and queued when it has no downloaded audio (custom
/// songs are skipped). Songs the user cancelled keep their pin and are not
/// queued again. Returns how many songs were queued.
///
/// # Errors
///
/// Returns storage errors from the lookups it performs.
pub async fn queue_kept_collections(
    storage: &Storage,
    queue: &DownloadQueue,
) -> Result<usize, StorageError> {
    let kept: Vec<_> = storage
        .download_collections()
        .await?
        .into_iter()
        .filter(|collection| collection.keep)
        .collect();
    if kept.is_empty() {
        return Ok(0);
    }
    let pins: HashSet<(String, String)> = storage.download_pins().await?.into_iter().collect();
    let mut records = Vec::new();
    for collection in kept {
        let Some(members) = storage
            .collection_members(collection.filter, &collection.owner)
            .await?
        else {
            continue;
        };
        let mut new_keys = Vec::new();
        for member in members {
            let Some(track) = storage.get_track(&member).await? else {
                continue;
            };
            if track.is_custom {
                continue;
            }
            let key = track.catalog_id.clone().unwrap_or_else(|| track.id.clone());
            if pins.contains(&(key.clone(), collection.owner.clone())) || new_keys.contains(&key) {
                continue;
            }
            new_keys.push(key);
            if !track.has_audio() {
                records.push(record_from_track(
                    &track,
                    Some((collection.kind, &collection.owner, &collection.name)),
                ));
            }
        }
        if !new_keys.is_empty() {
            tracing::info!(collection = %collection.owner, songs = new_keys.len(), "kept collection has new songs");
            storage
                .pin_downloads(Some(&collection.owner), new_keys)
                .await?;
        }
    }
    let count = records.len();
    if count > 0 && queue.enqueue(records).is_err() {
        return Ok(0);
    }
    Ok(count)
}

/// On-demand prefetch for a reference-audio custom song.
///
/// A custom song (`local-<n>`) with `audio_mode = 'reference'` plays by
/// dereferencing a real catalog track's file ([`crate::media::MediaResolver`]).
/// When that file is missing, this enqueues a download of the referenced
/// catalog id — the only id the DRM side ever sees — keyed on that id and
/// carrying the custom song's display metadata.
///
/// It is a no-op (returning `Ok(false)`) when `custom_song_id` is not a
/// reference custom song, when the referenced file is already present, or when
/// a download for that catalog id is already queued. Returns `Ok(true)` when a
/// new download was enqueued.
///
/// # Errors
///
/// Returns storage errors from the lookups it performs.
pub async fn enqueue_missing_reference(
    storage: &Storage,
    queue: &DownloadQueue,
    custom_song_id: &str,
) -> Result<bool, StorageError> {
    if !custom_song_id.starts_with(CUSTOM_SONG_PREFIX) {
        return Ok(false);
    }
    let Some(track) = storage.get_track(custom_song_id).await? else {
        return Ok(false);
    };
    if !track.is_custom || track.audio_mode != Some(CustomAudioMode::Reference) {
        return Ok(false);
    }
    let Some(reference) = track.reference_track_id.clone() else {
        return Ok(false);
    };

    // Already have the referenced file? Nothing to fetch.
    let resolver = MediaResolver::new(storage.clone());
    if resolver.resolve(&reference).await?.is_some() {
        return Ok(false);
    }

    // Already queued (by catalog id)? Don't pile on a duplicate.
    let existing = storage.list_downloads().await?;
    if existing.iter().any(|row| row.track_id == reference) {
        return Ok(false);
    }

    let record = DownloadRecord {
        id: reference.clone(),
        track_id: reference.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration_ms: track.duration_ms.unwrap_or(0),
        artwork_url: track.artwork_url.clone(),
        source_kind: DownloadSourceKind::Song,
        source_id: None,
        source_name: None,
        status: DownloadStatus::Queued,
        progress: 0.0,
        message: None,
        deferred: false,
        replace: false,
        created_at: 0,
        updated_at: 0,
    };
    // The queue actor may already have stopped during shutdown; that is not a
    // storage error, so swallow it and report "not enqueued".
    if queue.enqueue(vec![record]).is_err() {
        return Ok(false);
    }
    tracing::info!(song = %custom_song_id, catalog = %reference, "reference-audio prefetch enqueued");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enqueue_without_downloader_marks_failed() {
        let storage = Storage::open_in_memory().unwrap();
        let registry = Arc::new(DownloaderRegistry::new()); // no "drm" registered
        let queue = DownloadQueue::spawn(
            storage.clone(),
            registry,
            PathBuf::from("."),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let mut events = queue.events();

        let record = DownloadRecord {
            id: "101".into(),
            track_id: "101".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: None,
            duration_ms: 0,
            artwork_url: None,
            source_kind: DownloadSourceKind::Song,
            source_id: None,
            source_name: None,
            status: DownloadStatus::Queued,
            progress: 0.0,
            message: None,
            deferred: false,
            replace: false,
            created_at: 0,
            updated_at: 0,
        };
        queue.enqueue(vec![record]).unwrap();

        // Drain until we see a Failed status for t1.
        let mut saw_failed = false;
        for _ in 0..20 {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let DownloadEvent::StatusChanged { id, status, .. } = evt
                && id == "101"
                && status == DownloadStatus::Failed
            {
                saw_failed = true;
                break;
            }
        }
        assert!(
            saw_failed,
            "download should have failed without a drm downloader"
        );

        let row = storage.get_download("101").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
    }

    #[tokio::test]
    async fn clear_finished_removes_done_rows() {
        let storage = Storage::open_in_memory().unwrap();
        let registry = Arc::new(DownloaderRegistry::new());
        let queue = DownloadQueue::spawn(
            storage.clone(),
            registry,
            PathBuf::from("."),
            Arc::new(AtomicU64::new(0)),
            None,
        );

        let mut record = DownloadRecord {
            id: "done1".into(),
            track_id: "done1".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: None,
            duration_ms: 0,
            artwork_url: None,
            source_kind: DownloadSourceKind::Song,
            source_id: None,
            source_name: None,
            status: DownloadStatus::Done,
            progress: 1.0,
            message: None,
            deferred: false,
            replace: false,
            created_at: 1,
            updated_at: 1,
        };
        storage.upsert_download(record.clone()).await.unwrap();
        record.id = "queued1".into();
        record.track_id = "queued1".into();
        record.status = DownloadStatus::Queued;
        record.progress = 0.0;
        storage.upsert_download(record).await.unwrap();

        queue.clear_finished().unwrap();
        // Let the actor process the command.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(storage.get_download("done1").await.unwrap().is_none());
        assert!(storage.get_download("queued1").await.unwrap().is_some());
    }

    // ----- Control (pause / resume / cancel) + progress + reference prefetch -----

    use std::sync::atomic::AtomicUsize;
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use crate::download::{DownloadError, Downloader};
    use crate::storage::{CustomAudio, NewCustomTrack, TrackMapping};

    /// A downloader that signals when its fetch starts and then blocks on a gate
    /// until the test releases it, so pause / cancel can hit an in-flight fetch.
    struct GatedDownloader {
        started: mpsc::UnboundedSender<()>,
        gate: Arc<Notify>,
    }

    #[async_trait]
    impl Downloader for GatedDownloader {
        fn id(&self) -> &'static str {
            DOWNLOADER_ID
        }

        async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
            let _ = self.started.send(());
            // Honours the cancel token like a real downloader.
            tokio::select! {
                () = request.cancel.cancelled() => return Err(DownloadError::Cancelled),
                () = self.gate.notified() => {}
            }
            write_output(request).await
        }
    }

    /// Writes a placeholder file at the request's output path.
    async fn write_output(request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
        if let Some(dir) = request.output_path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&request.output_path, b"x").await?;
        Ok(Downloaded::new(
            request.output_path.clone(),
            request.variant,
        ))
    }

    /// A downloader that reports two progress samples and then succeeds.
    struct ProgressDownloader {
        reports: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Downloader for ProgressDownloader {
        fn id(&self) -> &'static str {
            DOWNLOADER_ID
        }

        async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
            if let Some(progress) = &request.progress {
                progress.report(50, Some(100));
                progress.report(100, Some(100));
                self.reports.store(2, Ordering::SeqCst);
            }
            write_output(request).await
        }
    }

    fn queued_record(id: &str) -> DownloadRecord {
        DownloadRecord {
            id: id.into(),
            track_id: id.into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: None,
            duration_ms: 0,
            artwork_url: None,
            source_kind: DownloadSourceKind::Song,
            source_id: None,
            source_name: None,
            status: DownloadStatus::Queued,
            progress: 0.0,
            message: None,
            deferred: false,
            replace: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn next_event(events: &mut broadcast::Receiver<DownloadEvent>) -> DownloadEvent {
        tokio::time::timeout(StdDuration::from_secs(5), events.recv())
            .await
            .expect("event within timeout")
            .expect("broadcast still open")
    }

    async fn wait_status(
        events: &mut broadcast::Receiver<DownloadEvent>,
        want_id: &str,
        want: DownloadStatus,
    ) {
        for _ in 0..200 {
            if let DownloadEvent::StatusChanged { id, status, .. } = next_event(events).await
                && id == want_id
                && status == want
            {
                return;
            }
        }
        panic!("did not observe {want:?} for {want_id}");
    }

    async fn wait_removed(events: &mut broadcast::Receiver<DownloadEvent>, want_id: &str) {
        for _ in 0..200 {
            if let DownloadEvent::Removed(id) = next_event(events).await
                && id == want_id
            {
                return;
            }
        }
        panic!("did not observe Removed for {want_id}");
    }

    #[tokio::test]
    async fn pause_should_hold_then_resume_requeues() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Notify::new());
        let mut registry = DownloaderRegistry::new();
        registry.register(GatedDownloader {
            started: started_tx,
            gate: gate.clone(),
        });
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let mut events = queue.events();

        queue.enqueue(vec![queued_record("101")]).unwrap();
        // First fetch is running.
        started_rx.recv().await.unwrap();

        // Pause aborts the in-flight fetch and holds the row.
        queue.pause("101").unwrap();
        wait_status(&mut events, "101", DownloadStatus::Paused).await;
        assert_eq!(
            storage.get_download("101").await.unwrap().unwrap().status,
            DownloadStatus::Paused
        );

        // Resume re-queues and a worker re-runs the fetch from scratch.
        queue.resume("101").unwrap();
        started_rx.recv().await.unwrap();
        wait_status(&mut events, "101", DownloadStatus::Downloading).await;

        // Release the gate so the second fetch completes.
        gate.notify_one();
        wait_status(&mut events, "101", DownloadStatus::Done).await;
        assert_eq!(
            storage.get_download("101").await.unwrap().unwrap().status,
            DownloadStatus::Done
        );
    }

    #[tokio::test]
    async fn cancel_should_abort_in_flight_and_remove_row() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Notify::new());
        let mut registry = DownloaderRegistry::new();
        registry.register(GatedDownloader {
            started: started_tx,
            gate,
        });
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let mut events = queue.events();

        queue.enqueue(vec![queued_record("101")]).unwrap();
        started_rx.recv().await.unwrap();

        queue.cancel("101").unwrap();
        wait_removed(&mut events, "101").await;
        assert!(storage.get_download("101").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn progress_reports_should_persist() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut registry = DownloaderRegistry::new();
        registry.register(ProgressDownloader {
            reports: Arc::new(AtomicUsize::new(0)),
        });
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let mut events = queue.events();
        let mut progress = queue.progress();

        queue.enqueue(vec![queued_record("101")]).unwrap();

        // The minimal UI stream carries an intermediate fraction.
        let mut saw_mid = false;
        for _ in 0..200 {
            let update = tokio::time::timeout(StdDuration::from_secs(5), progress.recv())
                .await
                .unwrap()
                .unwrap();
            if update.id == "101" && (0.4..0.6).contains(&update.progress) && !update.failed {
                saw_mid = true;
                break;
            }
            if update.id == "101" && update.status == DownloadStatus::Done {
                break;
            }
        }
        assert!(saw_mid, "expected an intermediate ~0.5 progress update");

        wait_status(&mut events, "101", DownloadStatus::Done).await;
        let row = storage.get_download("101").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Done);
        assert!((row.progress - 1.0).abs() < f32::EPSILON);
    }

    /// Records every lyrics request and optionally fails it.
    struct RecordingLyrics {
        calls: mpsc::UnboundedSender<String>,
        fail: bool,
    }

    #[async_trait]
    impl LyricsFetcher for RecordingLyrics {
        async fn fetch_lyrics(
            &self,
            track_id: &str,
            _info: &TrackInfo,
        ) -> Result<Option<LyricsSource>, LibraryError> {
            let _ = self.calls.send(track_id.to_owned());
            if self.fail {
                Err(LibraryError::UnknownTrack(track_id.to_owned()))
            } else {
                Ok(Some(LyricsSource::Lrclib))
            }
        }
    }

    /// Runs one successful download with a [`RecordingLyrics`] attached and
    /// returns the lyrics request it produced plus the final row status.
    async fn download_with_lyrics(fail: bool) -> (String, DownloadStatus) {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut registry = DownloaderRegistry::new();
        registry.register(ProgressDownloader {
            reports: Arc::new(AtomicUsize::new(0)),
        });
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            Some(Arc::new(RecordingLyrics {
                calls: calls_tx,
                fail,
            })),
        );
        queue.enqueue(vec![queued_record("101")]).unwrap();
        let requested = tokio::time::timeout(StdDuration::from_secs(5), calls_rx.recv())
            .await
            .expect("lyrics fetch within timeout")
            .expect("lyrics channel open");
        // Give a (wrongly) failing lyrics step a chance to touch the row.
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        let status = storage.get_download("101").await.unwrap().unwrap().status;
        (requested, status)
    }

    #[tokio::test]
    async fn completed_download_should_trigger_lyrics_fetch() {
        let (requested, _) = download_with_lyrics(false).await;
        assert_eq!(requested, "101");
    }

    #[tokio::test]
    async fn lyrics_failure_should_leave_download_done() {
        let (_, status) = download_with_lyrics(true).await;
        assert_eq!(status, DownloadStatus::Done);
    }

    #[tokio::test]
    async fn failed_download_should_not_fetch_lyrics() {
        let storage = Storage::open_in_memory().unwrap();
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()), // no "drm": the download fails
            PathBuf::from("."),
            Arc::new(AtomicU64::new(0)),
            Some(Arc::new(RecordingLyrics {
                calls: calls_tx,
                fail: false,
            })),
        );
        let mut events = queue.events();
        queue.enqueue(vec![queued_record("101")]).unwrap();
        wait_status(&mut events, "101", DownloadStatus::Failed).await;
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert!(calls_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reference_prefetch_should_enqueue_missing_catalog() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let mut events = queue.events();

        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Ref".into(),
                artist: "Artist".into(),
                audio: CustomAudio::Reference("999".into()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();

        let enqueued = enqueue_missing_reference(&storage, &queue, &id)
            .await
            .unwrap();
        assert!(enqueued, "a missing reference file should be enqueued");

        // The queued row keys on the catalog id, not the local- id.
        for _ in 0..200 {
            if let DownloadEvent::Added(record) = next_event(&mut events).await
                && record.track_id == "999"
            {
                return;
            }
        }
        panic!("reference prefetch did not enqueue catalog 999");
    }

    #[tokio::test]
    async fn reference_prefetch_should_skip_when_file_present() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );

        // The referenced catalog track already has a local file.
        let file = dir.path().join("555.m4a");
        tokio::fs::write(&file, b"x").await.unwrap();
        storage
            .set_track_mapping(TrackMapping {
                track_id: "555".into(),
                file_path: file,
            })
            .await
            .unwrap();
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Ref".into(),
                artist: "Artist".into(),
                audio: CustomAudio::Reference("555".into()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();

        let enqueued = enqueue_missing_reference(&storage, &queue, &id)
            .await
            .unwrap();
        assert!(!enqueued, "present file should not be re-fetched");
        assert!(storage.get_download("555").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reference_prefetch_should_skip_non_reference_song() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Local".into(),
                artist: "Artist".into(),
                audio: CustomAudio::None,
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();

        let enqueued = enqueue_missing_reference(&storage, &queue, &id)
            .await
            .unwrap();
        assert!(!enqueued, "a non-reference custom song must not prefetch");
    }

    // ----- Pins: Apple Music style "Remove download" -----

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    #[tokio::test]
    async fn shared_song_should_survive_removing_one_playlist() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .pin_downloads(Some("p.a"), ids(&["1", "2"]))
            .await
            .unwrap();
        storage
            .pin_downloads(Some("p.b"), ids(&["2", "3"]))
            .await
            .unwrap();
        let orphans = storage
            .unpin_downloads(Some("p.a"), ids(&["1", "2"]))
            .await
            .unwrap();
        assert_eq!(orphans, ids(&["1"]), "song 2 is still needed by p.b");
    }

    #[tokio::test]
    async fn shared_song_should_go_when_both_playlists_are_removed() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .pin_downloads(Some("p.a"), ids(&["2"]))
            .await
            .unwrap();
        storage
            .pin_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();
        storage
            .unpin_downloads(Some("p.a"), ids(&["2"]))
            .await
            .unwrap();
        let orphans = storage
            .unpin_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();
        assert_eq!(orphans, ids(&["2"]));
    }

    #[tokio::test]
    async fn individually_downloaded_song_should_survive_removing_its_album() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .pin_downloads(Some("l.album"), ids(&["7", "8"]))
            .await
            .unwrap();
        storage.pin_downloads(None, ids(&["8"])).await.unwrap();
        let orphans = storage
            .unpin_downloads(Some("l.album"), ids(&["7", "8"]))
            .await
            .unwrap();
        assert_eq!(
            orphans,
            ids(&["7"]),
            "song 8 was also downloaded on its own"
        );
    }

    /// Queue + media dir with an audio file and a track row for each id.
    async fn queue_with_files(files: &[&str]) -> (Storage, DownloadQueue, tempfile::TempDir) {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().to_path_buf();
        for id in files {
            let path = media.join(format!("{id}.m4a"));
            std::fs::write(&path, b"x").unwrap();
            let mut track = crate::storage::Track::minimal(*id, "Song", "Artist");
            track.audio = crate::storage::TrackAudio::normal(path);
            storage.upsert_track(track).await.unwrap();
        }
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            media,
            Arc::new(AtomicU64::new(0)),
            None,
        );
        (storage, queue, dir)
    }

    #[tokio::test]
    async fn removing_one_playlist_should_keep_shared_file_and_delete_the_rest() {
        let (storage, queue, dir) = queue_with_files(&["1", "2"]).await;
        storage
            .pin_downloads(Some("p.a"), ids(&["1", "2"]))
            .await
            .unwrap();
        storage
            .pin_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();

        let removed = queue
            .remove_downloads(Some("p.a"), ids(&["1", "2"]))
            .await
            .unwrap();

        assert_eq!(removed, ids(&["1"]));
        assert!(!dir.path().join("1.m4a").exists());
        assert!(dir.path().join("2.m4a").exists());
        assert!(
            storage
                .get_track("1")
                .await
                .unwrap()
                .unwrap()
                .audio
                .is_empty()
        );
    }

    #[tokio::test]
    async fn removing_both_playlists_should_delete_the_shared_file() {
        let (storage, queue, dir) = queue_with_files(&["2"]).await;
        storage
            .pin_downloads(Some("p.a"), ids(&["2"]))
            .await
            .unwrap();
        storage
            .pin_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();
        queue
            .remove_downloads(Some("p.a"), ids(&["2"]))
            .await
            .unwrap();
        assert!(dir.path().join("2.m4a").exists());
        queue
            .remove_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();
        assert!(!dir.path().join("2.m4a").exists());
    }

    #[tokio::test]
    async fn removing_album_should_keep_individually_downloaded_song() {
        let (storage, queue, dir) = queue_with_files(&["7", "8"]).await;
        storage
            .pin_downloads(Some("l.album"), ids(&["7", "8"]))
            .await
            .unwrap();
        storage.pin_downloads(None, ids(&["8"])).await.unwrap();
        queue
            .remove_downloads(Some("l.album"), ids(&["7", "8"]))
            .await
            .unwrap();
        assert!(!dir.path().join("7.m4a").exists());
        assert!(dir.path().join("8.m4a").exists());
    }

    #[tokio::test]
    async fn removing_a_single_song_should_delete_it_even_if_pinned_by_a_playlist() {
        let (storage, queue, dir) = queue_with_files(&["5"]).await;
        storage
            .pin_downloads(Some("p.a"), ids(&["5"]))
            .await
            .unwrap();
        let removed = queue.remove_downloads(None, ids(&["5"])).await.unwrap();
        assert_eq!(removed, ids(&["5"]));
        assert!(!dir.path().join("5.m4a").exists());
        assert_eq!(
            storage.download_owners("5").await.unwrap(),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn removing_a_custom_playlist_should_keep_custom_and_referenced_audio() {
        let (storage, queue, dir) = queue_with_files(&["1", "3"]).await;
        let own = storage
            .create_custom_track(NewCustomTrack {
                title: "Mine".into(),
                artist: "Me".into(),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let own_file = dir.path().join(format!("{own}.flac"));
        std::fs::write(&own_file, b"mine").unwrap();
        storage
            .set_custom_track_audio(&own, CustomAudio::LocalCopy(own_file.clone()))
            .await
            .unwrap();
        storage
            .create_custom_track(NewCustomTrack {
                title: "Ref".into(),
                artist: "Me".into(),
                audio: CustomAudio::Reference("3".into()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        storage
            .pin_downloads(Some("pl.custom"), ids(&["1", "3"]))
            .await
            .unwrap();
        let keys = ids(&[own.as_str(), "1", "3"]);
        let preview = storage
            .download_removal_preview("pl.custom", keys.clone())
            .await
            .unwrap();

        let removed = queue
            .remove_downloads(Some("pl.custom"), keys)
            .await
            .unwrap();

        assert_eq!(removed, ids(&["1"]));
        assert_eq!(
            (preview.deleted.len(), preview.kept.len()),
            (1, 1),
            "the dialog counts what really happens"
        );
        assert!(
            own_file.exists(),
            "a custom song's own audio is never removed"
        );
        assert!(
            !storage
                .get_track(&own)
                .await
                .unwrap()
                .unwrap()
                .audio
                .is_empty()
        );
        assert!(dir.path().join("3.m4a").exists(), "a custom song plays it");
        assert!(!dir.path().join("1.m4a").exists());
    }

    /// Removes the downloads of collection `CIDER_REMOVE_OWNER` from a COPY of
    /// a real database (never the live file; opening it migrates it), the
    /// way the library's "Remove downloads" does, twice, and reports what
    /// happened. Files are looked for in the copy's `media` folder. Run with
    /// `cargo test -p cider-offline-core real_db_copy_remove -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs CIDER_DB_COPY (a copy of a real store.sqlite) and CIDER_REMOVE_OWNER"]
    #[expect(
        clippy::print_stdout,
        reason = "a manual check that reports what it saw"
    )]
    async fn real_db_copy_remove_collection() {
        let path =
            std::path::PathBuf::from(std::env::var_os("CIDER_DB_COPY").expect("CIDER_DB_COPY"));
        let owner = std::env::var("CIDER_REMOVE_OWNER").expect("CIDER_REMOVE_OWNER");
        let storage = Storage::open_async(path.clone()).await.unwrap();
        let media = path.parent().unwrap().join("media");
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            media.clone(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let filter = crate::storage::LibraryFilter::AppleMusic;
        let members = storage
            .collection_members(filter, &owner)
            .await
            .unwrap()
            .unwrap_or_default();
        let mut keys: Vec<String> = Vec::new();
        for member in &members {
            if let Some(track) = storage.get_track(member).await.unwrap()
                && !track.is_custom
            {
                let key = track.catalog_id.unwrap_or(track.id);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        let pinned_before = storage.pinned_collections().await.unwrap().contains(&owner);
        let preview = storage
            .download_removal_preview(&owner, keys.clone())
            .await
            .unwrap();
        println!(
            "members {} keys {} preview (deleted, kept) {preview:?} pinned before {pinned_before}",
            members.len(),
            keys.len(),
        );
        let first = queue
            .remove_downloads(Some(&owner), keys.clone())
            .await
            .unwrap();
        let second = queue.remove_downloads(Some(&owner), keys).await.unwrap();
        let (_, report) = crate::download_cleanup::unlinked_downloads(&storage, &media)
            .await
            .unwrap();
        let pinned_after = storage.pinned_collections().await.unwrap().contains(&owner);
        println!(
            "removed first {} second {} pinned after {pinned_after} unlinked files {}",
            first.len(),
            second.len(),
            report.files
        );
    }

    #[tokio::test]
    async fn removing_a_collection_should_drop_every_pin_it_holds() {
        let (storage, queue, _dir) = queue_with_files(&["1", "2"]).await;
        // `2` left the playlist upstream but is still pinned by it, like the
        // owner's 0.1.0 pin keyed on a library id.
        storage
            .pin_downloads(Some("p.mix"), ids(&["1", "2"]))
            .await
            .unwrap();
        let removed = queue
            .remove_downloads(Some("p.mix"), ids(&["1"]))
            .await
            .unwrap();
        assert_eq!(
            (removed, storage.pinned_collections().await.unwrap()),
            (ids(&["1", "2"]), Vec::<String>::new())
        );
    }

    #[tokio::test]
    async fn removing_downloads_twice_should_report_nothing_the_second_time() {
        let (storage, queue, _dir) = queue_with_files(&["1"]).await;
        storage
            .pin_downloads(Some("p.mix"), ids(&["1"]))
            .await
            .unwrap();
        queue
            .remove_downloads(Some("p.mix"), ids(&["1"]))
            .await
            .unwrap();
        let again = queue
            .remove_downloads(Some("p.mix"), ids(&["1"]))
            .await
            .unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[tokio::test]
    async fn removing_a_song_by_its_library_id_should_remove_the_one_file() {
        let (storage, queue, dir) = queue_with_files(&["1"]).await;
        let mut library = crate::storage::Track::minimal("i.one", "Song", "Artist");
        library.catalog_id = Some("1".into());
        library.audio = crate::storage::TrackAudio::normal(dir.path().join("1.m4a"));
        storage.upsert_track(library).await.unwrap();
        let removed = queue.remove_downloads(None, ids(&["i.one"])).await.unwrap();
        let song = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!(
            (
                removed.len(),
                song.has_audio(),
                dir.path().join("1.m4a").exists()
            ),
            (1, false, false)
        );
    }

    #[tokio::test]
    async fn existing_media_files_should_be_linked_at_startup() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("42.m4a"), b"x").unwrap();
        let mut track = crate::storage::Track::minimal("i.lib", "Song", "Artist");
        track.catalog_id = Some("42".into());
        storage.upsert_track(track).await.unwrap();
        let _queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        for _ in 0..100 {
            let track = storage.get_track("i.lib").await.unwrap().unwrap();
            if track.has_audio() {
                // A linked file gets an individual pin, so it never counts
                // as an unlinked download (e.g. after a database reset).
                for _ in 0..100 {
                    if storage.download_owners("42").await.unwrap() == [String::new()] {
                        return;
                    }
                    tokio::time::sleep(StdDuration::from_millis(10)).await;
                }
                panic!("the linked file was not pinned");
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("media file was not linked to the library track");
    }

    // ----- Unavailable / retry policy -----

    #[tokio::test]
    async fn music_video_should_be_marked_unavailable_without_fetching() {
        let storage = Storage::open_in_memory().unwrap();
        let mut track = crate::storage::Track::minimal("i.mv", "KING", "Artist");
        track.catalog_id = Some("6782267818".into());
        track.metadata = Some(
            r#"{"type":"library-music-videos","attributes":{"playParams":{"kind":"musicVideo"}}}"#
                .into(),
        );
        storage.upsert_track(track).await.unwrap();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let mut registry = DownloaderRegistry::new();
        registry.register(GatedDownloader {
            started: started_tx,
            gate: Arc::new(Notify::new()),
        });
        let queue = DownloadQueue::spawn(
            storage.clone(),
            Arc::new(registry),
            PathBuf::from("."),
            Arc::new(AtomicU64::new(0)),
            None,
        );
        let mut events = queue.events();
        queue.enqueue(vec![queued_record("6782267818")]).unwrap();
        loop {
            if let DownloadEvent::Added(record) = next_event(&mut events).await {
                assert_eq!(record.status, DownloadStatus::Unavailable);
                break;
            }
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert!(
            started_rx.try_recv().is_err(),
            "a music video must not be fetched"
        );
    }

    // ----- Typed error -> queue behaviour -----

    #[test]
    fn unavailable_error_should_never_be_retried() {
        assert!(matches!(
            classify(DownloadError::unavailable("no ctrp asset")),
            FetchOutcome::Unavailable(_)
        ));
    }

    #[test]
    fn transient_error_should_be_retried_with_retry_after() {
        let error = DownloadError::transient_http(
            429,
            Some(StdDuration::from_secs(30)),
            "license HTTP 429, 0 bytes",
        );
        assert!(matches!(
            classify(error),
            FetchOutcome::Failed {
                retryable: true,
                retry_after: Some(after),
                ..
            } if after == StdDuration::from_secs(30)
        ));
    }

    #[test]
    fn auth_expired_should_fail_without_retry() {
        assert!(matches!(
            classify(DownloadError::auth_expired("webPlayback HTTP 401")),
            FetchOutcome::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn cancelled_error_should_leave_the_row_to_the_actor() {
        assert!(matches!(
            classify(DownloadError::Cancelled),
            FetchOutcome::Cancelled
        ));
    }

    // ----- Progress: the downloader's reports beat the estimate -----

    #[tokio::test]
    async fn reported_stage_should_override_the_estimated_stage() {
        // A 1 ms estimate puts the estimated stage in Fetching after 150 ms.
        let mut state = ProgressState::new(StdDuration::from_millis(1));
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        state.apply(ProgressReport::Stage(DownloadStage::License));
        assert_eq!(state.current().0, DownloadStage::License);
    }

    #[test]
    fn reported_bytes_should_set_the_fetch_fraction() {
        let mut state = ProgressState::new(StdDuration::from_hours(1));
        state.apply(ProgressReport::Bytes {
            done: 50,
            total: Some(100),
        });
        let (phase, fraction) = state.current();
        assert_eq!(
            (phase, (fraction * 100.0).round()),
            (DownloadStage::Fetching, 50.0)
        );
    }

    // ----- Resolution, capabilities, twins -----

    /// Catalog search that finds nothing (tests never reach Apple).
    struct NoSearch;

    #[async_trait]
    impl TwinSearch for NoSearch {
        async fn search_songs(
            &self,
            _term: &str,
        ) -> Result<Vec<crate::catalog::SongCandidate>, crate::catalog::CatalogError> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl SpatialLookup for NoSearch {
        async fn spatial_master(
            &self,
            _catalog_id: &str,
        ) -> Result<Option<String>, crate::catalog::CatalogError> {
            Ok(None)
        }
    }

    /// Records every request and writes `<output_path>`; declares
    /// `music_videos` as configured.
    struct RecordingDownloader {
        requests: mpsc::UnboundedSender<DownloadRequest>,
        music_videos: bool,
    }

    #[async_trait]
    impl Downloader for RecordingDownloader {
        fn id(&self) -> &'static str {
            DOWNLOADER_ID
        }

        fn capabilities(&self) -> DownloaderCapabilities {
            DownloaderCapabilities::new().with_music_videos(self.music_videos)
        }

        async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
            let _ = self.requests.send(request.clone());
            write_output(request).await
        }
    }

    /// Runs one record through a queue with a [`RecordingDownloader`] and
    /// returns its final status and the request the downloader saw.
    async fn run_recorded(
        storage: &Storage,
        record: DownloadRecord,
        music_videos: bool,
    ) -> (DownloadStatus, Option<DownloadRequest>) {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut registry = DownloaderRegistry::new();
        registry.register(RecordingDownloader {
            requests: tx,
            music_videos,
        });
        let queue = DownloadQueue::spawn_with(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
            fast_timing(),
            Arc::new(NoSearch),
        );
        let mut events = queue.events();
        let id = record.id.clone();
        queue.enqueue(vec![record]).unwrap();
        for _ in 0..500 {
            let status = match next_event(&mut events).await {
                DownloadEvent::Added(row)
                    if row.id == id && row.status == DownloadStatus::Unavailable =>
                {
                    Some(row.status)
                }
                DownloadEvent::StatusChanged {
                    id: row, status, ..
                } if row == id
                    && matches!(
                        status,
                        DownloadStatus::Done | DownloadStatus::Failed | DownloadStatus::Unavailable
                    ) =>
                {
                    Some(status)
                }
                _ => None,
            };
            if let Some(status) = status {
                return (status, rx.try_recv().ok());
            }
        }
        panic!("download never finished");
    }

    fn library_song(
        id: &str,
        catalog_id: Option<&str>,
        play_params: bool,
    ) -> crate::storage::Track {
        let mut track = crate::storage::Track::minimal(id, "LATE NIGHT", "Example Artist");
        track.catalog_id = catalog_id.map(str::to_owned);
        track.album = Some("$ome $exy $ongs 4 U".into());
        track.duration_ms = Some(195_432);
        track.track_number = Some(15);
        let params = match (play_params, catalog_id) {
            (true, Some(catalog)) => {
                format!(r#","playParams":{{"catalogId":"{catalog}","kind":"song"}}"#)
            }
            _ => String::new(),
        };
        track.metadata = Some(format!(
            r#"{{"id":"{id}","attributes":{{"name":"LATE NIGHT"{params}}}}}"#
        ));
        track
    }

    #[tokio::test]
    async fn library_only_item_should_download_its_twin_as_a_library_play() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_track(library_song("i.FakeLibSong0001", None, false))
            .await
            .unwrap();
        storage
            .upsert_track(library_song("a.6814096109", Some("6814096109"), true))
            .await
            .unwrap();
        let (status, request) =
            run_recorded(&storage, queued_record("i.FakeLibSong0001"), false).await;
        assert_eq!(
            (status, request.map(|r| r.playback)),
            (
                DownloadStatus::Done,
                Some(crate::download::PlaybackId::library(
                    "a.6814096109",
                    "6814096109"
                ))
            )
        );
    }

    #[tokio::test]
    async fn downloaded_twin_should_mark_the_original_as_downloaded() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_track(library_song("i.FakeLibSong0001", None, false))
            .await
            .unwrap();
        storage
            .upsert_track(library_song("a.6814096109", Some("6814096109"), true))
            .await
            .unwrap();
        run_recorded(&storage, queued_record("i.FakeLibSong0001"), false).await;
        let original = storage
            .get_track("i.FakeLibSong0001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (original.catalog_id.as_deref(), original.has_audio()),
            (Some("6814096109"), true)
        );
    }

    #[tokio::test]
    async fn library_only_item_without_twin_should_be_unavailable() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_track(library_song("i.FakeLibSong0001", None, false))
            .await
            .unwrap();
        let (status, request) =
            run_recorded(&storage, queued_record("i.FakeLibSong0001"), false).await;
        assert_eq!(
            (status, request.is_none()),
            (DownloadStatus::Unavailable, true)
        );
    }

    #[tokio::test]
    async fn catalog_song_should_be_a_catalog_play() {
        let storage = Storage::open_in_memory().unwrap();
        let (_, request) = run_recorded(&storage, queued_record("959563981"), false).await;
        assert_eq!(
            request.map(|r| r.playback),
            Some(crate::download::PlaybackId::catalog("959563981"))
        );
    }

    fn music_video_track() -> crate::storage::Track {
        let mut track = crate::storage::Track::minimal("i.mv", "KING", "Artist");
        track.catalog_id = Some("6782267818".into());
        track.metadata = Some(
            r#"{"type":"library-music-videos","attributes":{"playParams":{"catalogId":"6782267818","kind":"musicVideo"}}}"#
                .into(),
        );
        track
    }

    #[tokio::test]
    async fn music_video_should_be_fetched_once_the_downloader_supports_it() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_track(music_video_track()).await.unwrap();
        let (status, request) = run_recorded(&storage, queued_record("6782267818"), true).await;
        assert_eq!(
            (status, request.map(|r| (r.kind, r.playback))),
            (
                DownloadStatus::Done,
                Some((
                    MediaKind::MusicVideo,
                    crate::download::PlaybackId::catalog("6782267818")
                ))
            )
        );
    }

    #[tokio::test]
    async fn album_version_should_be_queued_as_its_own_row() {
        let storage = Storage::open_in_memory().unwrap();
        let mut video = music_video_track();
        video.album = Some("Example Album (Deluxe)".into());
        storage.upsert_track(video).await.unwrap();
        let mut song = crate::storage::Track::minimal("6782267800", "KING", "Artist");
        song.album = Some("Example Album (Deluxe)".into());
        storage.upsert_track(song).await.unwrap();
        let queue = DownloadQueue::spawn_with(
            storage.clone(),
            Arc::new(DownloaderRegistry::new()),
            PathBuf::from("."),
            Arc::new(AtomicU64::new(0)),
            None,
            fast_timing(),
            Arc::new(NoSearch),
        );
        let queued = queue.download_album_version("6782267818").await.unwrap();
        assert_eq!(queued.as_deref(), Some("6782267800"));
    }

    #[test]
    fn other_error_should_be_retryable() {
        let error = DownloadError::Other(
            "Widevine license error: bad license JSON: EOF while parsing a value".into(),
        );
        assert!(matches!(
            classify(error),
            FetchOutcome::Failed {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn backoff_should_grow_and_stay_jittered() {
        let first = backoff_delay(BACKOFF_BASE, 2, "x");
        let second = backoff_delay(BACKOFF_BASE, 3, "x");
        assert!((3_000..=5_000).contains(&first.as_millis()), "{first:?}");
        assert!((9_000..=15_000).contains(&second.as_millis()), "{second:?}");
    }

    #[test]
    fn backoff_should_be_capped() {
        assert!(backoff_delay(BACKOFF_BASE, 20, "x") <= BACKOFF_MAX * 5 / 4);
    }

    /// Fails with a temporary error `failures` times, then succeeds.
    struct FlakyDownloader {
        failures: AtomicUsize,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Downloader for FlakyDownloader {
        fn id(&self) -> &'static str {
            DOWNLOADER_ID
        }

        async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let left = self.failures.load(Ordering::SeqCst);
            if left > 0 {
                self.failures.store(left - 1, Ordering::SeqCst);
                return Err(DownloadError::Other(
                    "Widevine license error: bad license JSON: EOF".into(),
                ));
            }
            write_output(request).await
        }
    }

    fn fast_timing() -> Timing {
        Timing {
            spacing: StdDuration::from_millis(5),
            cooldown: StdDuration::from_millis(5),
            backoff_base: StdDuration::from_millis(20),
            cancel_grace: StdDuration::from_secs(2),
        }
    }

    async fn run_flaky(failures: usize) -> (DownloadStatus, usize) {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = DownloaderRegistry::new();
        registry.register(FlakyDownloader {
            failures: AtomicUsize::new(failures),
            calls: Arc::clone(&calls),
        });
        let queue = DownloadQueue::spawn_with(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
            fast_timing(),
            Arc::new(NoSearch),
        );
        let mut events = queue.events();
        queue.enqueue(vec![queued_record("11")]).unwrap();
        for _ in 0..500 {
            if let DownloadEvent::StatusChanged { id, status, .. } = next_event(&mut events).await
                && id == "11"
                && matches!(status, DownloadStatus::Done | DownloadStatus::Failed)
            {
                return (status, calls.load(Ordering::SeqCst));
            }
        }
        panic!("download never finished");
    }

    #[tokio::test]
    async fn temporary_failure_should_be_retried_until_success() {
        assert_eq!(run_flaky(2).await, (DownloadStatus::Done, 3));
    }

    #[tokio::test]
    async fn retries_should_stop_after_max_attempts() {
        let max = usize::try_from(MAX_ATTEMPTS).unwrap();
        assert_eq!(run_flaky(10).await, (DownloadStatus::Failed, max));
    }

    #[tokio::test]
    async fn concurrency_limit_should_bound_running_fetches() {
        let gate = Arc::new(Gate::new(2));
        let a = gate.acquire().await;
        let _b = gate.acquire().await;
        let third = tokio::time::timeout(StdDuration::from_millis(50), gate.acquire()).await;
        assert!(third.is_err(), "a third slot must wait");
        drop(a);
        let third = tokio::time::timeout(StdDuration::from_millis(500), gate.acquire()).await;
        assert!(third.is_ok(), "a freed slot must be handed out");
    }

    #[tokio::test]
    async fn raising_the_limit_should_wake_waiters() {
        let gate = Arc::new(Gate::new(1));
        let _a = gate.acquire().await;
        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                let _permit = gate.acquire().await;
            }
        });
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        gate.set_limit(2);
        tokio::time::timeout(StdDuration::from_millis(500), waiter)
            .await
            .expect("waiter woken")
            .unwrap();
    }

    // ----- Download format (normal / spatial) -----

    const MASTER: &str = "https://aod.itunes.apple.com/itunes-assets/x/P1_default.m3u8";

    /// Catalog stand-in whose songs have (or lack) a spatial master.
    struct FakeCatalog {
        master: Option<&'static str>,
        lookups: AtomicUsize,
    }

    #[async_trait]
    impl TwinSearch for FakeCatalog {
        async fn search_songs(
            &self,
            _term: &str,
        ) -> Result<Vec<crate::catalog::SongCandidate>, crate::catalog::CatalogError> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl SpatialLookup for FakeCatalog {
        async fn spatial_master(
            &self,
            _catalog_id: &str,
        ) -> Result<Option<String>, crate::catalog::CatalogError> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            Ok(self.master.map(str::to_owned))
        }
    }

    /// Records `(variant, enhanced_hls)` of every request; a spatial request
    /// fails with `VariantUnavailable` unless `has_binaural`.
    /// `(variant, enhanced_hls)` of every request a downloader saw.
    type Seen = Arc<Mutex<Vec<(AudioVariant, Option<String>)>>>;

    struct VariantDownloader {
        seen: Seen,
        spatial: bool,
        has_binaural: bool,
    }

    #[async_trait]
    impl Downloader for VariantDownloader {
        fn id(&self) -> &'static str {
            DOWNLOADER_ID
        }

        fn capabilities(&self) -> DownloaderCapabilities {
            DownloaderCapabilities::new().with_spatial(self.spatial)
        }

        async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((request.variant, request.enhanced_hls.clone()));
            }
            if request.variant == AudioVariant::Spatial && !self.has_binaural {
                return Err(DownloadError::variant_unavailable(AudioVariant::Spatial));
            }
            write_output(request)
                .await
                .map(|d| d.with_duration_ms(Some(221_564)))
        }
    }

    struct FormatRun {
        status: DownloadStatus,
        requests: Vec<(AudioVariant, Option<String>)>,
        files: Vec<crate::storage::AudioFile>,
        lookups: usize,
        /// Whether the song has a spatial mix, as the queue recorded it.
        learned: Option<bool>,
    }

    async fn run_format(
        format: DownloadFormat,
        master: Option<&'static str>,
        spatial: bool,
        has_binaural: bool,
    ) -> FormatRun {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_track(crate::storage::Track::minimal("31", "Song", "Artist"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = DownloaderRegistry::new();
        registry.register(VariantDownloader {
            seen: Arc::clone(&seen),
            spatial,
            has_binaural,
        });
        let catalog = Arc::new(FakeCatalog {
            master,
            lookups: AtomicUsize::new(0),
        });
        let queue = DownloadQueue::spawn_with(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
            fast_timing(),
            Arc::clone(&catalog),
        );
        queue.set_download_format(format);
        let mut events = queue.events();
        queue.enqueue(vec![queued_record("31")]).unwrap();
        let status = wait_final(&mut events, "31").await;
        let requests = seen.lock().unwrap().clone();
        FormatRun {
            status,
            requests,
            files: storage.track_audio_files("31").await.unwrap(),
            lookups: catalog.lookups.load(Ordering::SeqCst),
            learned: storage.song_spatial().await.unwrap().get("31").copied(),
        }
    }

    #[tokio::test]
    async fn a_song_without_a_master_should_be_learned_as_having_no_spatial_mix() {
        let run = run_format(DownloadFormat::Both, None, true, true).await;
        assert_eq!(run.learned, Some(false));
    }

    #[tokio::test]
    async fn a_downloaded_spatial_mix_should_be_learned() {
        let run = run_format(DownloadFormat::Both, Some(MASTER), true, true).await;
        assert_eq!(run.learned, Some(true));
    }

    #[tokio::test]
    async fn a_master_without_binaural_should_be_learned_as_no_spatial_mix() {
        let run = run_format(DownloadFormat::Both, Some(MASTER), true, false).await;
        assert_eq!(run.learned, Some(false));
    }

    #[tokio::test]
    async fn stereo_only_song_known_without_spatial_should_need_no_lookup() {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("31.m4a");
        std::fs::write(&path, b"x").unwrap();
        let mut track = crate::storage::Track::minimal("31", "Song", "Artist");
        track.audio = crate::storage::TrackAudio::normal(path);
        storage.upsert_track(track).await.unwrap();
        storage.set_song_spatial("31", false).await.unwrap();
        let mut registry = DownloaderRegistry::new();
        registry.register(VariantDownloader {
            seen: Arc::new(Mutex::new(Vec::new())),
            spatial: true,
            has_binaural: true,
        });
        let catalog = Arc::new(FakeCatalog {
            master: Some(MASTER),
            lookups: AtomicUsize::new(0),
        });
        let queue = DownloadQueue::spawn_with(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
            fast_timing(),
            Arc::clone(&catalog),
        );
        queue.set_download_format(DownloadFormat::Both);
        let mut events = queue.events();
        queue.enqueue(vec![queued_record("31")]).unwrap();
        wait_final(&mut events, "31").await;
        assert_eq!(catalog.lookups.load(Ordering::SeqCst), 0);
    }

    /// The first terminal status of `want_id`.
    async fn wait_final(
        events: &mut broadcast::Receiver<DownloadEvent>,
        want_id: &str,
    ) -> DownloadStatus {
        for _ in 0..400 {
            if let DownloadEvent::StatusChanged { id, status, .. } = next_event(events).await
                && id == want_id
                && matches!(
                    status,
                    DownloadStatus::Done | DownloadStatus::Failed | DownloadStatus::Unavailable
                )
            {
                return status;
            }
        }
        panic!("no final status for {want_id}");
    }

    fn variants(files: &[crate::storage::AudioFile]) -> Vec<AudioVariant> {
        files.iter().map(|f| f.variant).collect()
    }

    #[tokio::test]
    async fn normal_format_should_fetch_only_normal_without_a_lookup() {
        let run = run_format(DownloadFormat::Stereo, Some(MASTER), true, true).await;
        assert_eq!(
            (run.requests, run.lookups),
            (vec![(AudioVariant::Stereo, None)], 0)
        );
    }

    #[tokio::test]
    async fn spatial_where_available_should_fetch_only_spatial() {
        let run = run_format(
            DownloadFormat::SpatialWhereAvailable,
            Some(MASTER),
            true,
            true,
        )
        .await;
        assert_eq!(
            (run.status, run.requests, variants(&run.files)),
            (
                DownloadStatus::Done,
                vec![(AudioVariant::Spatial, Some(MASTER.to_owned()))],
                vec![AudioVariant::Spatial]
            )
        );
    }

    #[tokio::test]
    async fn spatial_where_available_should_fall_back_to_stereo_without_binaural() {
        let run = run_format(
            DownloadFormat::SpatialWhereAvailable,
            Some(MASTER),
            true,
            false,
        )
        .await;
        assert_eq!(
            (run.status, variants(&run.files)),
            (DownloadStatus::Done, vec![AudioVariant::Stereo])
        );
    }

    #[tokio::test]
    async fn spatial_where_available_should_fetch_normal_when_the_song_has_no_master() {
        let run = run_format(DownloadFormat::SpatialWhereAvailable, None, true, true).await;
        assert_eq!(run.requests, vec![(AudioVariant::Stereo, None)]);
    }

    #[tokio::test]
    async fn both_format_should_record_both_files() {
        let run = run_format(DownloadFormat::Both, Some(MASTER), true, true).await;
        assert_eq!(
            (run.status, variants(&run.files)),
            (
                DownloadStatus::Done,
                vec![AudioVariant::Stereo, AudioVariant::Spatial]
            )
        );
    }

    #[tokio::test]
    async fn both_format_should_keep_normal_when_the_song_lacks_binaural() {
        let run = run_format(DownloadFormat::Both, Some(MASTER), true, false).await;
        assert_eq!(
            (run.status, variants(&run.files)),
            (DownloadStatus::Done, vec![AudioVariant::Stereo])
        );
    }

    #[tokio::test]
    async fn spatial_should_not_be_requested_from_a_downloader_without_support() {
        let run = run_format(DownloadFormat::Both, Some(MASTER), false, true).await;
        assert_eq!(
            (run.requests, run.lookups),
            (vec![(AudioVariant::Stereo, None)], 0)
        );
    }

    #[tokio::test]
    async fn recorded_files_should_carry_size_and_duration() {
        let run = run_format(
            DownloadFormat::SpatialWhereAvailable,
            Some(MASTER),
            true,
            true,
        )
        .await;
        let file = &run.files[0];
        assert_eq!(
            (file.size, file.duration_ms, file.path.file_name()),
            (
                Some(1),
                Some(221_564),
                Some(std::ffi::OsStr::new("31.spatial.m4a"))
            )
        );
    }

    #[test]
    fn plan_should_follow_the_format() {
        let spatial = |fall_back_to_stereo| Step {
            variant: AudioVariant::Spatial,
            fall_back_to_stereo,
        };
        assert_eq!(
            [
                plan_steps(DownloadFormat::Stereo, true),
                plan_steps(DownloadFormat::SpatialWhereAvailable, true),
                plan_steps(DownloadFormat::SpatialWhereAvailable, false),
                plan_steps(DownloadFormat::Both, true),
                plan_steps(DownloadFormat::Both, false),
            ],
            [
                vec![Step::STEREO],
                vec![spatial(true)],
                vec![Step::STEREO],
                vec![Step::STEREO, spatial(false)],
                vec![Step::STEREO],
            ]
        );
    }

    #[test]
    fn step_span_should_split_the_ring_between_both_mixes() {
        assert_eq!([step_span(0, 2), step_span(1, 2)], [(0.0, 0.5), (0.5, 1.0)]);
    }

    #[test]
    fn step_span_should_give_a_stereo_fallback_the_whole_ring() {
        // Spatial where available: spatial was missing, stereo is the only
        // version left to fetch.
        assert_eq!(step_span(0, 1), (0.0, 1.0));
    }

    #[test]
    fn step_span_should_keep_the_share_of_a_mix_written_before_a_retry() {
        assert_eq!(step_span(1, 2), (0.5, 1.0));
    }

    #[test]
    fn stereo_on_disk_should_satisfy_a_stereo_only_download() {
        assert!(all_wanted_present(
            DownloadFormat::Stereo,
            true,
            &[AudioVariant::Stereo],
            None
        ));
    }

    #[test]
    fn stereo_on_disk_should_not_satisfy_a_both_download() {
        assert!(!all_wanted_present(
            DownloadFormat::Both,
            true,
            &[AudioVariant::Stereo],
            None
        ));
    }

    #[test]
    fn stereo_on_disk_should_satisfy_both_for_a_song_known_without_spatial() {
        assert!(all_wanted_present(
            DownloadFormat::Both,
            true,
            &[AudioVariant::Stereo],
            Some(false)
        ));
    }

    #[test]
    fn spatial_on_disk_should_satisfy_spatial_where_available() {
        assert!(all_wanted_present(
            DownloadFormat::SpatialWhereAvailable,
            true,
            &[AudioVariant::Spatial],
            None
        ));
    }

    #[test]
    fn step_span_should_never_run_past_the_end_of_the_ring() {
        assert_eq!(step_span(1, 1), (0.5, 1.0));
    }

    // ----- Download management: merge, cancel, pins, unlinked downloads -----

    /// A downloader whose work runs on a task of its own, like the real one's
    /// blocking decrypt and save: dropping the fetch future does not stop it.
    /// After a cancel it needs `stop_delay` to wind down. It counts fetches
    /// and the most fetches it ever ran at once.
    struct DetachedDownloader {
        started: mpsc::UnboundedSender<()>,
        gate: Arc<Notify>,
        in_flight: Arc<AtomicUsize>,
        most_at_once: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        stop_delay: StdDuration,
        /// After a cancel, finish the file anyway (a save that was already
        /// under way) instead of stopping.
        complete_after_cancel: Arc<AtomicBool>,
    }

    impl DetachedDownloader {
        fn new(started: mpsc::UnboundedSender<()>, gate: Arc<Notify>) -> Self {
            Self {
                started,
                gate,
                in_flight: Arc::new(AtomicUsize::new(0)),
                most_at_once: Arc::new(AtomicUsize::new(0)),
                calls: Arc::new(AtomicUsize::new(0)),
                stop_delay: StdDuration::from_millis(300),
                complete_after_cancel: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl Downloader for DetachedDownloader {
        fn id(&self) -> &'static str {
            DOWNLOADER_ID
        }

        async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_at_once.fetch_max(now, Ordering::SeqCst);
            let _ = self.started.send(());
            let gate = Arc::clone(&self.gate);
            let cancel = request.cancel.clone();
            let in_flight = Arc::clone(&self.in_flight);
            let stop_delay = self.stop_delay;
            let output = request.output_path.clone();
            let variant = request.variant;
            let complete_after_cancel = self.complete_after_cancel.load(Ordering::SeqCst);
            tokio::spawn(async move {
                let result = tokio::select! {
                    () = cancel.cancelled() => {
                        tokio::time::sleep(stop_delay).await;
                        if complete_after_cancel {
                            tokio::fs::write(&output, b"x")
                                .await
                                .map(|()| Downloaded::new(output.clone(), variant))
                                .map_err(DownloadError::from)
                        } else {
                            Err(DownloadError::Cancelled)
                        }
                    }
                    () = gate.notified() => tokio::fs::write(&output, b"x")
                        .await
                        .map(|()| Downloaded::new(output.clone(), variant))
                        .map_err(DownloadError::from),
                };
                in_flight.fetch_sub(1, Ordering::SeqCst);
                result
            })
            .await
            .map_err(|error| DownloadError::Other(error.to_string()))?
        }
    }

    struct Harness {
        storage: Storage,
        queue: DownloadQueue,
        started: mpsc::UnboundedReceiver<()>,
        gate: Arc<Notify>,
        most_at_once: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        in_flight: Arc<AtomicUsize>,
        complete_after_cancel: Arc<AtomicBool>,
        dir: tempfile::TempDir,
    }

    fn harness(concurrency: usize) -> Harness {
        let storage = Storage::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (started_tx, started) = mpsc::unbounded_channel();
        let gate = Arc::new(Notify::new());
        let downloader = DetachedDownloader::new(started_tx, Arc::clone(&gate));
        let most_at_once = Arc::clone(&downloader.most_at_once);
        let calls = Arc::clone(&downloader.calls);
        let in_flight = Arc::clone(&downloader.in_flight);
        let complete_after_cancel = Arc::clone(&downloader.complete_after_cancel);
        let mut registry = DownloaderRegistry::new();
        registry.register(downloader);
        let queue = DownloadQueue::spawn_with(
            storage.clone(),
            Arc::new(registry),
            dir.path().to_path_buf(),
            Arc::new(AtomicU64::new(0)),
            None,
            fast_timing(),
            Arc::new(NoSearch),
        );
        queue.set_concurrency(concurrency);
        Harness {
            storage,
            queue,
            started,
            gate,
            most_at_once,
            calls,
            in_flight,
            complete_after_cancel,
            dir,
        }
    }

    impl Harness {
        /// Records a stereo file of song `id` on disk and in the store.
        async fn with_file(&self, id: &str) -> PathBuf {
            let path = self.dir.path().join(format!("{id}.m4a"));
            std::fs::write(&path, b"old").unwrap();
            let mut track = crate::storage::Track::minimal(id, "Song", "Artist");
            track.audio = crate::storage::TrackAudio::normal(path.clone());
            self.storage.upsert_track(track).await.unwrap();
            path
        }

        async fn row(&self, id: &str) -> Option<DownloadRecord> {
            self.storage.get_download(id).await.unwrap()
        }

        /// Waits until the actor has handled every command sent so far.
        async fn settle(&self) {
            // `download_album_version` answers through the actor, after the
            // commands queued before it.
            let _ = self.queue.download_album_version("none").await;
        }
    }

    fn redownload(id: &str) -> DownloadRecord {
        DownloadRecord {
            replace: true,
            ..queued_record(id)
        }
    }

    #[tokio::test]
    async fn download_again_after_pause_should_never_run_two_fetches_of_one_song() {
        // The reported duplicate: pausing a running download and downloading
        // it again started a second fetch while the first one was still
        // winding down (decrypting / saving on its own task).
        let mut h = harness(2);
        let mut events = h.queue.events();
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        h.started.recv().await.unwrap();
        h.queue.pause("7").unwrap();
        wait_status(&mut events, "7", DownloadStatus::Paused).await;
        h.queue.enqueue(vec![redownload("7")]).unwrap();
        h.started.recv().await.unwrap();
        h.gate.notify_one();
        wait_status(&mut events, "7", DownloadStatus::Done).await;
        assert_eq!(h.most_at_once.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rapid_repeated_downloads_should_fetch_a_song_once() {
        let mut h = harness(4);
        let mut events = h.queue.events();
        h.queue
            .enqueue(vec![
                queued_record("7"),
                queued_record("7"),
                redownload("7"),
            ])
            .unwrap();
        for _ in 0..5 {
            h.queue.enqueue(vec![queued_record("7")]).unwrap();
            h.queue.enqueue(vec![redownload("7")]).unwrap();
        }
        h.started.recv().await.unwrap();
        h.settle().await;
        h.gate.notify_one();
        wait_status(&mut events, "7", DownloadStatus::Done).await;
        h.settle().await;
        assert_eq!(
            (
                h.calls.load(Ordering::SeqCst),
                h.most_at_once.load(Ordering::SeqCst),
                h.storage.list_downloads().await.unwrap().len()
            ),
            (1, 1, 1)
        );
    }

    #[tokio::test]
    async fn download_of_a_song_already_on_disk_should_fetch_nothing() {
        let h = harness(2);
        h.with_file("7").await;
        let mut events = h.queue.events();
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        wait_status(&mut events, "7", DownloadStatus::Done).await;
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn redownload_should_fetch_a_song_already_on_disk_again() {
        let mut h = harness(2);
        let path = h.with_file("7").await;
        let mut events = h.queue.events();
        h.queue.enqueue(vec![redownload("7")]).unwrap();
        h.started.recv().await.unwrap();
        h.gate.notify_one();
        wait_status(&mut events, "7", DownloadStatus::Done).await;
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"x",
            "the new file replaced the old one"
        );
    }

    #[tokio::test]
    async fn redownload_merged_into_a_waiting_download_should_upgrade_it_to_replace() {
        let mut h = harness(1);
        h.with_file("7").await;
        let mut events = h.queue.events();
        // "1" holds the only slot, so "7" waits as a plain download.
        h.queue.enqueue(vec![queued_record("1")]).unwrap();
        h.started.recv().await.unwrap();
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        h.queue.enqueue(vec![redownload("7")]).unwrap();
        h.settle().await;
        assert!(h.row("7").await.unwrap().replace);
        h.gate.notify_one();
        wait_status(&mut events, "1", DownloadStatus::Done).await;
        h.started.recv().await.unwrap();
        h.gate.notify_one();
        wait_status(&mut events, "7", DownloadStatus::Done).await;
        assert_eq!(h.calls.load(Ordering::SeqCst), 2, "7 was fetched again");
    }

    #[tokio::test]
    async fn download_of_a_paused_song_should_resume_it() {
        let mut h = harness(2);
        let mut events = h.queue.events();
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        h.started.recv().await.unwrap();
        h.queue.pause("7").unwrap();
        wait_status(&mut events, "7", DownloadStatus::Paused).await;
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        h.started.recv().await.unwrap();
        assert_eq!(
            h.row("7").await.unwrap().status,
            DownloadStatus::Downloading
        );
    }

    #[tokio::test]
    async fn pause_should_stick_while_the_fetch_winds_down() {
        let mut h = harness(2);
        let mut events = h.queue.events();
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        h.started.recv().await.unwrap();
        h.queue.pause("7").unwrap();
        wait_status(&mut events, "7", DownloadStatus::Paused).await;
        // Longer than the downloader needs to stop.
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        assert_eq!(h.row("7").await.unwrap().status, DownloadStatus::Paused);
    }

    #[tokio::test]
    async fn cancel_during_a_redownload_should_keep_the_complete_file() {
        let mut h = harness(2);
        let path = h.with_file("7").await;
        let mut events = h.queue.events();
        h.queue.enqueue(vec![redownload("7")]).unwrap();
        h.started.recv().await.unwrap();
        h.queue.cancel("7").unwrap();
        wait_removed(&mut events, "7").await;
        tokio::time::sleep(StdDuration::from_millis(400)).await;
        let recorded = h.storage.track_audio_files("7").await.unwrap();
        assert_eq!(
            (std::fs::read(&path).unwrap(), recorded.len()),
            (b"old".to_vec(), 1)
        );
    }

    #[tokio::test]
    async fn remove_downloads_should_delete_a_file_that_finished_while_cancelling() {
        let mut h = harness(1);
        h.complete_after_cancel.store(true, Ordering::SeqCst);
        h.storage
            .upsert_track(crate::storage::Track::minimal("4", "Song", "Artist"))
            .await
            .unwrap();
        h.storage
            .pin_downloads(Some("p.a"), ids(&["4"]))
            .await
            .unwrap();
        h.queue.enqueue(vec![queued_record("4")]).unwrap();
        h.started.recv().await.unwrap();

        // The cancelled fetch still saves its file; the removal must wait for
        // that and delete it, instead of the worker recording it afterwards.
        h.queue
            .remove_downloads(Some("p.a"), ids(&["4"]))
            .await
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(200)).await;

        assert!(!h.dir.path().join("4.m4a").exists());
        assert_eq!(h.storage.track_audio_files("4").await.unwrap().len(), 0);
    }

    #[test]
    fn redownload_merged_while_finishing_should_run_the_worker_again() {
        let flags = WorkerFlags::new(false);
        assert!(matches!(after_finish(true, &flags), AttemptEnd::Finished));
        // Merged after the first check, while the row was being finished.
        flags.upgraded.store(true, Ordering::SeqCst);
        assert!(matches!(after_finish(true, &flags), AttemptEnd::Again));
        assert!(matches!(after_finish(true, &flags), AttemptEnd::Finished));
        // Nothing was skipped: every mix is fresh, nothing to replace.
        flags.upgraded.store(true, Ordering::SeqCst);
        assert!(matches!(after_finish(false, &flags), AttemptEnd::Finished));
    }

    #[tokio::test]
    async fn shutdown_should_stop_running_workers_and_keep_their_rows() {
        let mut h = harness(1);
        h.queue.enqueue(vec![queued_record("6")]).unwrap();
        h.started.recv().await.unwrap();

        h.queue.shutdown(StdDuration::from_secs(5)).await;

        assert_eq!(
            h.in_flight.load(Ordering::SeqCst),
            0,
            "no fetch is left running"
        );
        assert_eq!(
            h.row("6").await.map(|row| row.status),
            Some(DownloadStatus::Downloading),
            "the next queue resumes the row"
        );
        assert!(h.queue.enqueue(vec![queued_record("7")]).is_err());
    }

    #[tokio::test]
    async fn cancel_all_should_remove_every_unfinished_row_and_keep_done_ones() {
        let mut h = harness(1);
        let mut done = queued_record("9");
        done.status = DownloadStatus::Done;
        h.storage.upsert_download(done).await.unwrap();
        h.queue
            .enqueue(vec![queued_record("1"), queued_record("2")])
            .unwrap();
        h.started.recv().await.unwrap();
        h.queue.pause("2").unwrap();
        h.queue.cancel_all().unwrap();
        h.settle().await;
        let left: Vec<String> = h
            .storage
            .list_downloads()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(left, ["9"]);
    }

    #[tokio::test]
    async fn cancel_should_release_the_pins_of_a_song_with_no_audio_but_keep_a_kept_collection() {
        let h = harness(1);
        h.storage
            .register_download_collection(
                crate::storage::DownloadCollection {
                    owner: "p.keep".to_owned(),
                    filter: crate::storage::LibraryFilter::AppleMusic,
                    kind: DownloadSourceKind::Playlist,
                    name: "Kept".to_owned(),
                    keep: false,
                },
                Some(true),
            )
            .await
            .unwrap();
        h.storage
            .pin_downloads(Some("p.keep"), ids(&["7"]))
            .await
            .unwrap();
        h.storage
            .pin_downloads(Some("p.other"), ids(&["7"]))
            .await
            .unwrap();
        h.queue.enqueue(vec![queued_record("7")]).unwrap();
        h.queue.cancel("7").unwrap();
        h.settle().await;
        assert_eq!(h.storage.download_owners("7").await.unwrap(), ["p.keep"]);
    }

    #[tokio::test]
    async fn stale_worker_writes_should_be_dropped() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_download(queued_record("7")).await.unwrap();
        let written = storage
            .update_download_status_if("7", DownloadStatus::Downloading, 0.5, None, || false)
            .await
            .unwrap();
        assert_eq!(
            (
                written,
                storage.get_download("7").await.unwrap().unwrap().status
            ),
            (false, DownloadStatus::Queued)
        );
    }

    /// A store with stereo files for songs `1`..=`4` in `media/`, where `1`
    /// is pinned on its own, `2` by the downloaded playlist `p.1` (of which
    /// it is still a member), `3` by `p.1` but no longer in it, and `4` has
    /// no pin. A custom song plays `4`'s file by reference only when
    /// `reference` is set.
    async fn unlinked_store(reference: bool) -> (Storage, DownloadQueue, tempfile::TempDir) {
        let (storage, queue, dir) = queue_with_files(&["1", "2", "3", "4"]).await;
        storage.pin_downloads(None, ids(&["1"])).await.unwrap();
        storage
            .pin_downloads(Some("p.1"), ids(&["2", "3"]))
            .await
            .unwrap();
        storage
            .register_download_collection(
                crate::storage::DownloadCollection {
                    owner: "p.1".to_owned(),
                    filter: crate::storage::LibraryFilter::AppleMusic,
                    kind: DownloadSourceKind::Playlist,
                    name: "List".to_owned(),
                    keep: false,
                },
                None,
            )
            .await
            .unwrap();
        storage
            .call(move |c| {
                c.execute_batch(
                    "INSERT INTO library_collections (id, kind, position, name, synced_at)
                       VALUES ('p.1', 'playlist', 0, 'List', 1);
                     INSERT INTO library_collection_tracks (collection_id, position, track_id)
                       VALUES ('p.1', 0, '2');",
                )?;
                if reference {
                    c.execute_batch(
                        "INSERT INTO tracks (id, title, artist, is_custom, audio_mode,
                                             reference_track_id, updated_at)
                           VALUES ('local-1', 'Mine', 'Me', 1, 'reference', '4', 1);",
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        (storage, queue, dir)
    }

    #[tokio::test]
    async fn unlinked_downloads_should_count_unpinned_and_stale_files() {
        let (storage, _queue, dir) = unlinked_store(false).await;
        let (files, report) = crate::download_cleanup::unlinked_downloads(&storage, dir.path())
            .await
            .unwrap();
        let mut keys: Vec<String> = files.into_iter().flat_map(|f| f.keys).collect();
        keys.sort();
        assert_eq!((keys, report.bytes), (ids(&["3", "4"]), 2));
    }

    #[tokio::test]
    async fn clean_up_should_delete_only_unlinked_files() {
        let (storage, queue, dir) = unlinked_store(true).await;
        let report = queue.clean_up_unlinked().await.unwrap();
        let on_disk: Vec<bool> = ["1", "2", "3", "4"]
            .iter()
            .map(|id| dir.path().join(format!("{id}.m4a")).exists())
            .collect();
        assert_eq!(
            (report.files, on_disk),
            (1, vec![true, true, false, true]),
            "3 left its playlist; 4 is played by a custom song"
        );
        assert_eq!(storage.track_audio_files("3").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn clean_up_should_drop_the_stale_pin() {
        let (storage, queue, _dir) = unlinked_store(false).await;
        queue.clean_up_unlinked().await.unwrap();
        assert_eq!(storage.download_owners("3").await.unwrap().len(), 0);
    }

    fn done_record(id: &str, track_id: &str) -> DownloadRecord {
        DownloadRecord {
            id: id.into(),
            track_id: track_id.into(),
            status: DownloadStatus::Done,
            progress: 1.0,
            ..queued_record(id)
        }
    }

    async fn row_ids(storage: &Storage) -> Vec<String> {
        let mut ids: Vec<String> = storage
            .list_downloads()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn clean_up_should_delete_the_finished_rows_of_deleted_songs() {
        let (storage, queue, _dir) = unlinked_store(false).await;
        for (id, track) in [("1", "1"), ("3", "3"), ("old.3", "3")] {
            storage
                .upsert_download(done_record(id, track))
                .await
                .unwrap();
        }
        queue.clean_up_unlinked().await.unwrap();
        assert_eq!(row_ids(&storage).await, ids(&["1"]));
    }

    #[tokio::test]
    async fn removing_a_collection_should_delete_every_row_of_its_deleted_songs() {
        let (storage, queue, _dir) = queue_with_files(&["1", "2"]).await;
        storage
            .pin_downloads(Some("p.a"), ids(&["1", "2"]))
            .await
            .unwrap();
        storage
            .pin_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();
        for (id, track) in [("1", "1"), ("old.1", "1"), ("2", "2")] {
            storage
                .upsert_download(done_record(id, track))
                .await
                .unwrap();
        }
        queue
            .remove_downloads(Some("p.a"), ids(&["1", "2"]))
            .await
            .unwrap();
        assert_eq!(
            row_ids(&storage).await,
            ids(&["2"]),
            "the shared song keeps its file and its row"
        );
    }

    #[tokio::test]
    async fn kept_collection_should_queue_only_songs_added_since() {
        let (storage, queue, _dir) = queue_with_files(&[]).await;
        storage
            .register_download_collection(
                crate::storage::DownloadCollection {
                    owner: "p.1".to_owned(),
                    filter: crate::storage::LibraryFilter::AppleMusic,
                    kind: DownloadSourceKind::Playlist,
                    name: "List".to_owned(),
                    keep: false,
                },
                Some(true),
            )
            .await
            .unwrap();
        for id in ["1", "2"] {
            storage
                .upsert_track(crate::storage::Track::minimal(id, "Song", "Artist"))
                .await
                .unwrap();
        }
        storage
            .pin_downloads(Some("p.1"), ids(&["1"]))
            .await
            .unwrap();
        storage
            .call(|c| {
                c.execute_batch(
                    "INSERT INTO library_collections (id, kind, position, name, synced_at)
                       VALUES ('p.1', 'playlist', 0, 'List', 1);
                     INSERT INTO library_collection_tracks (collection_id, position, track_id)
                       VALUES ('p.1', 0, '1'), ('p.1', 1, '2');",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let queued = queue_kept_collections(&storage, &queue).await.unwrap();
        assert_eq!(
            (queued, storage.download_owners("2").await.unwrap()),
            (1, vec!["p.1".to_owned()])
        );
    }

    #[tokio::test]
    async fn removal_preview_should_list_shared_songs_as_kept() {
        let (storage, _queue, _dir) = queue_with_files(&["1", "2", "3"]).await;
        storage
            .pin_downloads(Some("p.a"), ids(&["1", "2", "3", "4"]))
            .await
            .unwrap();
        storage
            .pin_downloads(Some("p.b"), ids(&["2"]))
            .await
            .unwrap();
        storage.pin_downloads(None, ids(&["3"])).await.unwrap();
        assert_eq!(
            storage
                .download_removal_preview("p.a", ids(&["1", "2", "3", "4"]))
                .await
                .unwrap(),
            crate::storage::RemovalPreview {
                deleted: ids(&["1"]),
                kept: ids(&["2", "3"]),
            }
        );
    }

    #[tokio::test]
    async fn removing_a_collection_download_should_forget_the_collection() {
        let (storage, queue, _dir) = unlinked_store(false).await;
        queue
            .remove_downloads(Some("p.1"), ids(&["2", "3"]))
            .await
            .unwrap();
        assert_eq!(storage.download_collections().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn old_partial_files_should_be_deleted_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join(".tmpAbC123");
        let song = dir.path().join("1.m4a");
        std::fs::write(&partial, b"half").unwrap();
        std::fs::write(&song, b"whole").unwrap();
        let old = std::time::SystemTime::now() - StdDuration::from_secs(3_600);
        std::fs::File::options()
            .write(true)
            .open(&partial)
            .unwrap()
            .set_modified(old)
            .unwrap();
        delete_stale_temp_files(dir.path()).await;
        assert_eq!((partial.exists(), song.exists()), (false, true));
    }
}
