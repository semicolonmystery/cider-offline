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

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::catalog::CatalogClient;
use crate::catalog::{CatalogItem, Song};
use crate::download::{
    DownloadError, DownloadProgress, DownloadRequest, DownloadStage, DownloaderCapabilities,
    DownloaderRegistry, MediaKind, ProgressReport, TrackInfo,
};
use crate::download_target::{self, MUSIC_VIDEO_REASON, Target, TwinSearch};
use crate::library::{Library, LibraryError};
use crate::media::{AUDIO_EXTENSIONS, CUSTOM_SONG_PREFIX, MediaResolver};
use crate::storage::{
    CustomAudioMode, DownloadRecord, DownloadSourceKind, DownloadStatus, LyricsSource, Storage,
    StorageError,
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
    /// Delete every `Done` row.
    ClearFinished,
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
}

/// Pacing of fetch starts and retries (shortened in tests).
#[derive(Debug, Clone, Copy)]
struct Timing {
    spacing: Duration,
    cooldown: Duration,
    backoff_base: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            spacing: START_SPACING,
            cooldown: FAILURE_COOLDOWN,
            backoff_base: BACKOFF_BASE,
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

/// Shared state between the actor and its worker tasks.
struct Inner {
    storage: Storage,
    downloaders: Arc<DownloaderRegistry>,
    media_dir: PathBuf,
    /// Catalog search used to find the twin of a library-only item.
    search: Arc<dyn TwinSearch>,
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
    /// Live workers by row id, each with the generation (`epoch`) that spawned
    /// it and the token that cancels its fetch. Pausing or cancelling a running
    /// row cancels its token; a worker only removes its own entry when the
    /// stored epoch still matches, so a re-dispatch under the same id is never
    /// clobbered.
    active: Mutex<HashMap<String, (u64, CancellationToken)>>,
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
}

impl std::fmt::Debug for DownloadQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadQueue")
            .field("concurrency", &self.gate.limit())
            .finish_non_exhaustive()
    }
}

impl DownloadQueue {
    /// Spawns the queue actor on the current tokio runtime with
    /// [`DEFAULT_CONCURRENCY`] (change it with [`Self::set_concurrency`]).
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
        let search = Arc::new(CatalogClient::new(storage.clone()));
        Self::spawn_with(
            storage,
            downloaders,
            media_dir,
            last_played_ts,
            lyrics,
            Timing::default(),
            search,
        )
    }

    fn spawn_with(
        storage: Storage,
        downloaders: Arc<DownloaderRegistry>,
        media_dir: PathBuf,
        last_played_ts: Arc<AtomicU64>,
        lyrics: Option<Arc<dyn LyricsFetcher>>,
        timing: Timing,
        search: Arc<dyn TwinSearch>,
    ) -> Self {
        let (events, _rx) = broadcast::channel(EVENTS_CAPACITY);
        let (progress_events, _prx) = broadcast::channel(PROGRESS_CAPACITY);
        let gate = Arc::new(Gate::new(DEFAULT_CONCURRENCY));
        let inner = Arc::new(Inner {
            storage,
            downloaders,
            media_dir,
            search,
            lyrics,
            gate: Arc::clone(&gate),
            timing,
            next_start: Mutex::new(Instant::now()),
            fetch_estimate_ms: AtomicU64::new(INITIAL_FETCH_ESTIMATE_MS),
            events: events.clone(),
            progress_events: progress_events.clone(),
            active: Mutex::new(HashMap::new()),
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
        }
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

    fn send(&self, cmd: Command) -> Result<(), QueueError> {
        self.cmd_tx.send(cmd).map_err(|_| QueueError::Closed)
    }
}

async fn run_actor(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Command>) {
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

async fn handle_command(inner: &Arc<Inner>, cmd: Command) {
    match cmd {
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

/// Registers a cancellation token for `record.id` (cancelling and replacing any
/// prior worker on the same id) and spawns its worker.
fn spawn_worker(inner: &Arc<Inner>, record: DownloadRecord) {
    let token = CancellationToken::new();
    let epoch = inner.epoch.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut map) = inner.active.lock()
        && let Some((_, previous)) = map.insert(record.id.clone(), (epoch, token.clone()))
    {
        previous.cancel();
    }
    tokio::spawn(run_worker(Arc::clone(inner), record, token, epoch));
}

/// Removes `id` from the active map only when the stored generation still
/// matches `epoch`, so a worker never evicts a newer re-dispatch of the same id.
fn clear_active(inner: &Arc<Inner>, id: &str, epoch: u64) {
    if let Ok(mut map) = inner.active.lock()
        && map.get(id).is_some_and(|(stored, _)| *stored == epoch)
    {
        map.remove(id);
    }
}

/// Cancels and removes the active worker token for `id`, if any.
fn cancel_active(inner: &Arc<Inner>, id: &str) {
    if let Ok(mut map) = inner.active.lock()
        && let Some((_, token)) = map.remove(id)
    {
        token.cancel();
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
        created_at: 0,
        updated_at: 0,
    };
    handle_enqueue(inner, record).await;
    Some(version.catalog_id)
}

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
    // Already waiting or running: keep that attempt instead of restarting it.
    if let Some(existing) = existing
        && matches!(
            existing.status,
            DownloadStatus::Queued | DownloadStatus::Downloading
        )
    {
        return;
    }
    record.status = DownloadStatus::Queued;
    record.progress = 0.0;
    record.message = None;
    if let Err(error) = inner.storage.upsert_download(record.clone()).await {
        tracing::warn!(id = %record.id, %error, "could not insert download row");
        return;
    }
    emit(&inner.events, DownloadEvent::Added(record.clone()));
    dispatch(inner, record);
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

async fn handle_cancel(inner: &Arc<Inner>, id: &str) {
    // Abort an in-flight (or slot-waiting) worker, and drop a deferred row
    // that has not been dispatched yet so the next drain cannot resurrect it.
    cancel_active(inner, id);
    if let Ok(mut set) = inner.pending_deferred.lock() {
        set.remove(id);
    }
    match inner.storage.delete_download(id).await {
        Ok(true) => emit(&inner.events, DownloadEvent::Removed(id.to_owned())),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%id, %error, "could not delete download");
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
        match inner.storage.unpin_downloads(Some(owner), track_ids).await {
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
    let mut removed = Vec::with_capacity(orphans.len());
    for id in orphans {
        handle_cancel(inner, &id).await;
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
    let mut files: Vec<PathBuf> = Vec::new();
    // A library item linked to a catalog twin shares the twin's file.
    let mut linked_catalog = None;
    if let Ok(Some(record)) = inner.storage.find_track(id).await {
        linked_catalog = record.catalog_id.filter(|catalog| catalog != id);
        files.extend(record.track.audio_path);
    }
    if crate::media::is_valid_media_id(id) {
        for ext in AUDIO_EXTENSIONS {
            files.push(inner.media_dir.join(format!("{id}.{ext}")));
        }
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
    if let Some(catalog) = &linked_catalog
        && let Err(error) = inner.storage.set_audio_by_any_id(catalog, None).await
    {
        tracing::warn!(%catalog, %error, "could not clear the linked audio path");
    }
    match inner.storage.set_audio_by_any_id(id, None).await {
        Ok(changed) => found || changed > 0,
        Err(error) => {
            tracing::warn!(%id, %error, "could not clear audio path");
            found
        }
    }
}

/// How one fetch attempt ended.
enum FetchOutcome {
    /// The worker's token fired (pause / cancel); the actor owns the row state.
    Cancelled,
    /// The file is on disk at `path`, fetched for `target`.
    Done { path: PathBuf, target: Target },
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

async fn run_worker(
    inner: Arc<Inner>,
    record: DownloadRecord,
    token: CancellationToken,
    epoch: u64,
) {
    let mut attempt = 1u32;
    loop {
        match run_attempt(&inner, &record, &token, attempt).await {
            AttemptEnd::Finished => break,
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
                if let Err(error) = inner
                    .storage
                    .update_download_status(
                        &record.id,
                        DownloadStatus::Queued,
                        0.0,
                        Some(note.clone()),
                    )
                    .await
                {
                    tracing::warn!(id = %record.id, %error, "could not record retry");
                }
                emit_live(
                    &inner,
                    &record.id,
                    DownloadStatus::Queued,
                    0.0,
                    None,
                    attempt,
                    Some(note),
                );
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

/// How one attempt of a worker ended.
enum AttemptEnd {
    /// Nothing more to do (done, failed for good, unavailable, cancelled).
    Finished,
    /// A temporary failure; retry after a backoff of at least `retry_after`.
    Retry {
        message: String,
        retry_after: Option<Duration>,
    },
}

async fn run_attempt(
    inner: &Arc<Inner>,
    record: &DownloadRecord,
    token: &CancellationToken,
    attempt: u32,
) -> AttemptEnd {
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
    if let Err(error) = inner
        .storage
        .update_download_status(&record.id, DownloadStatus::Downloading, start, None)
        .await
    {
        tracing::warn!(id = %record.id, %error, "could not mark download as running");
    }
    emit_live(
        inner,
        &record.id,
        DownloadStatus::Downloading,
        start,
        Some(DownloadStage::Requesting),
        attempt,
        None,
    );
    emit_summary(inner).await;

    let Some(downloader) = inner.downloaders.get(DOWNLOADER_ID) else {
        let message = Some(format!(
            "No {DOWNLOADER_ID:?} downloader is registered; drop a device.wvd file in the data \
             folder and restart."
        ));
        finish_failed(inner, &record.id, message).await;
        return AttemptEnd::Finished;
    };

    let started = Instant::now();
    let outcome = run_fetch(inner, record, token, attempt, downloader.as_ref()).await;
    drop(permit);
    match outcome {
        FetchOutcome::Cancelled => {
            tracing::debug!(id = %record.id, "download worker stopped by pause/cancel");
            AttemptEnd::Finished
        }
        FetchOutcome::Done { path, target } => {
            inner.record_fetch_time(started.elapsed());
            finish_done(inner, record, &path, &target).await;
            AttemptEnd::Finished
        }
        FetchOutcome::Unavailable(reason) => {
            tracing::info!(id = %record.id, %reason, "download not supported yet");
            finish_with(inner, &record.id, DownloadStatus::Unavailable, Some(reason)).await;
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
                finish_failed(inner, &record.id, Some(message)).await;
                AttemptEnd::Finished
            }
        }
    }
}

/// Live progress state of one fetch.
struct ProgressState {
    started: Instant,
    estimate: Duration,
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
    fn new(estimate: Duration) -> Self {
        let (start, _) = DownloadStage::Requesting.band();
        Self {
            started: Instant::now(),
            estimate,
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
        (stage, fraction.max(self.shown))
    }
}

/// Resolves what to fetch, builds the request, drives
/// [`crate::download::Downloader::fetch`] while publishing staged progress and
/// watching `token`, and returns the outcome.
async fn run_fetch(
    inner: &Arc<Inner>,
    record: &DownloadRecord,
    token: &CancellationToken,
    attempt: u32,
    downloader: &dyn crate::download::Downloader,
) -> FetchOutcome {
    let resolved = tokio::select! {
        biased;
        () = token.cancelled() => return FetchOutcome::Cancelled,
        resolved = download_target::resolve(&inner.storage, inner.search.as_ref(), &record.track_id) => resolved,
    };
    let target = match resolved {
        Ok(target) => target,
        Err(error) => return classify(error),
    };
    if target.kind == MediaKind::MusicVideo && !downloader.capabilities().music_videos {
        return FetchOutcome::Unavailable(MUSIC_VIDEO_REASON.to_owned());
    }
    tracing::debug!(id = %record.id, ?target, "download target resolved");

    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressReport>();
    let request = DownloadRequest::new(target.catalog_id.clone(), inner.media_dir.clone())
        .with_playback(target.playback.clone())
        .with_kind(target.kind)
        .with_info(record_info(record))
        .with_progress(DownloadProgress::new(progress_tx))
        .with_cancel(token.child_token());

    let fetch = downloader.fetch(&request);
    tokio::pin!(fetch);
    let mut state = ProgressState::new(inner.fetch_estimate());
    let mut tick = tokio::time::interval(PROGRESS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = token.cancelled() => break FetchOutcome::Cancelled,
            Some(report) = progress_rx.recv() => {
                tracing::debug!(id = %record.id, ?report, "downloader progress report");
                state.apply(report);
                publish_progress(inner, &record.id, attempt, &mut state).await;
            }
            _ = tick.tick() => publish_progress(inner, &record.id, attempt, &mut state).await,
            result = &mut fetch => break match result {
                Ok(path) => FetchOutcome::Done { path, target },
                // Only our token may cancel a fetch; anything else is a
                // downloader bug, so retry instead of leaving the row running.
                Err(DownloadError::Cancelled) if !token.is_cancelled() => FetchOutcome::failed(
                    "the downloader stopped without being cancelled".to_owned(),
                    true,
                ),
                Err(error) => classify(error),
            },
        }
    }
}

/// Publishes (and occasionally persists) the current staged progress.
async fn publish_progress(inner: &Arc<Inner>, id: &str, attempt: u32, state: &mut ProgressState) {
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
        if let Err(error) = inner
            .storage
            .update_download_status(id, DownloadStatus::Downloading, fraction, None)
            .await
        {
            tracing::warn!(%id, %error, "could not persist progress");
        }
    }
    emit_live(
        inner,
        id,
        DownloadStatus::Downloading,
        fraction,
        Some(phase),
        attempt,
        None,
    );
}

fn record_info(record: &DownloadRecord) -> TrackInfo {
    TrackInfo {
        title: record.title.clone(),
        artist: record.artist.clone(),
        album: record.album.clone(),
        duration_ms: (record.duration_ms != 0).then_some(record.duration_ms),
    }
}

/// Records a finished download: the file becomes the audio of every track row
/// with the fetched catalog id (and of the queued id, when that differs), a
/// library-only item is linked to the twin it was fetched as, the row turns
/// `Done`, and lyrics are fetched.
async fn finish_done(inner: &Arc<Inner>, record: &DownloadRecord, path: &Path, target: &Target) {
    let id = record.id.as_str();
    tracing::info!(%id, path = %path.display(), "download finished");
    emit_live(
        inner,
        id,
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
    for any_id in [target.catalog_id.as_str(), record.track_id.as_str()] {
        if let Err(error) = inner
            .storage
            .set_audio_by_any_id(any_id, Some(path.to_path_buf()))
            .await
        {
            tracing::warn!(%id, %error, "could not record the audio file");
        }
        if record.track_id == target.catalog_id {
            break;
        }
    }
    finish_with(inner, id, DownloadStatus::Done, None).await;
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

async fn finish_failed(inner: &Arc<Inner>, id: &str, message: Option<String>) {
    finish_with(inner, id, DownloadStatus::Failed, message).await;
}

/// Persists and publishes a terminal status (`Done`, `Failed`, `Unavailable`).
async fn finish_with(
    inner: &Arc<Inner>,
    id: &str,
    status: DownloadStatus,
    message: Option<String>,
) {
    let progress = if status == DownloadStatus::Done {
        1.0
    } else {
        0.0
    };
    if let Err(error) = inner
        .storage
        .update_download_status(id, status, progress, message.clone())
        .await
    {
        tracing::warn!(%id, %error, ?status, "could not record final status");
    }
    emit_status(inner, id, status, progress, message);
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
        // `0` lets storage fill both timestamps on insert.
        created_at: 0,
        updated_at: 0,
    }
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

        async fn fetch(&self, request: &DownloadRequest) -> Result<PathBuf, DownloadError> {
            let _ = self.started.send(());
            self.gate.notified().await;
            write_output(request).await
        }
    }

    /// Writes a placeholder file at the request's output path.
    async fn write_output(request: &DownloadRequest) -> Result<PathBuf, DownloadError> {
        if let Some(dir) = request.output_path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&request.output_path, b"x").await?;
        Ok(request.output_path.clone())
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

        async fn fetch(&self, request: &DownloadRequest) -> Result<PathBuf, DownloadError> {
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
            track.audio_path = Some(path);
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
                .audio_path
                .is_none()
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
        assert!(storage.download_owners("5").await.unwrap().is_empty());
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
            if track.audio_path.is_some() {
                return;
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

        async fn fetch(&self, request: &DownloadRequest) -> Result<PathBuf, DownloadError> {
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
            (
                original.catalog_id.as_deref(),
                original.audio_path.is_some()
            ),
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

        async fn fetch(&self, request: &DownloadRequest) -> Result<PathBuf, DownloadError> {
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
}
