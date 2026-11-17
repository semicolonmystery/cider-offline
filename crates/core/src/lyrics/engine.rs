//! Lyrics lookups across providers, with the user's failure policy.
//!
//! One lookup asks the enabled providers in priority order and keeps the
//! best lyrics seen (word-synced > line-synced > plain; on a tie the earlier
//! provider wins). It stops early once lyrics reach the target quality, and
//! skips providers that cannot beat what it already has. A provider failure (rate limit,
//! 5xx, timeout, network) is handled per provider by its
//! [`FailurePolicy`]: wait and retry the same song, skip it and retry after
//! the batch, or skip it. "No lyrics" is never a failure.
//!
//! Every provider has a gate: requests to it are spaced (LRCLIB:
//! `lrclib_spacing_ms`), and after a failure it cools down for its
//! `Retry-After` (or [`IMPLICIT_COOLDOWN`]). Songs that would hit a cooling
//! provider under a skip policy are skipped without a request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use tokio::time::Instant;

use super::{
    FailurePolicy, FetchedLyrics, LyricsKind, LyricsProvider, LyricsProviderId, LyricsQuality,
    LyricsQuery, LyricsRequest, LyricsSettings, LyricsStore, ProviderError, ttml_quality,
};
use crate::download::TrackInfo;
use crate::library::LibraryError;
use crate::storage::{LyricsSource, Storage, TrackRecord};

/// Waits of [`FailurePolicy::WaitAndRetry`], one per retry of the same song.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];
/// Cool-down of a provider after a failure that named no `Retry-After`.
pub(crate) const IMPLICIT_COOLDOWN: Duration = Duration::from_secs(2);
/// Longest wait honoured for a `Retry-After`.
const MAX_WAIT: Duration = Duration::from_mins(5);
/// Wait before the first retry pass; each later pass waits
/// [`PASS_FACTOR`] times longer.
const PASS_BASE: Duration = Duration::from_secs(10);
/// Growth of the wait between retry passes.
const PASS_FACTOR: u32 = 3;

/// Outcome of a bulk lyrics lookup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LyricsReport {
    /// Songs that gained (or already had) lyrics.
    pub found: usize,
    /// Songs that already had lyrics and kept them (included in `found`).
    pub kept: usize,
    /// Newly found word-synced lyrics.
    pub word: usize,
    /// Newly found line-synced lyrics.
    pub line: usize,
    /// Newly found plain lyrics.
    pub plain: usize,
    /// Newly found lyrics per provider, indexed by
    /// [`LyricsProviderId::index`] (see [`Self::from`]).
    pub by_provider: [usize; LyricsProviderId::ALL.len()],
    /// Songs no provider has lyrics for.
    pub missing: usize,
    /// Songs whose lookup failed (after every retry the policy allows).
    pub failed: usize,
    /// Songs skipped on a failure and found in a later retry pass (also
    /// counted in `found`).
    pub recovered: usize,
    /// Retry passes run.
    pub passes: u8,
}

impl LyricsReport {
    /// Newly found lyrics from `provider`.
    pub fn from(&self, provider: LyricsProviderId) -> usize {
        self.by_provider[provider.index()]
    }
}

/// Live state of a bulk lyrics lookup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LyricsProgress {
    /// Songs in the batch.
    pub total: usize,
    /// Songs looked up in the current pass.
    pub checked: usize,
    /// Songs in the current pass.
    pub pass_size: usize,
    /// Songs with lyrics so far.
    pub found: usize,
    /// Songs without lyrics so far.
    pub missing: usize,
    /// Songs skipped on a failure, waiting for a retry pass.
    pub deferred: usize,
    /// Songs whose lookup failed for good.
    pub failed: usize,
    /// Current retry pass (`0` while the batch itself runs).
    pub pass: u8,
    /// Retry passes allowed.
    pub passes: u8,
    /// Set while waiting before a retry pass.
    pub waiting: Option<Duration>,
}

/// How a lookup was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LookupMode {
    /// One song, on request: always looks up (existing non-Apple lyrics are
    /// replaced), never defers (a skip policy fails fast).
    Explicit,
    /// Part of a batch: keeps existing lyrics unless "overwrite" is on.
    Bulk,
    /// After a download: like [`Self::Bulk`], deferred songs are retried
    /// once the download queue is idle.
    Download,
    /// A retry pass over songs skipped on a failure (batch or post-download):
    /// a cooling provider is waited for instead of skipping the song
    /// unasked; a new failure defers the song again.
    Retry,
}

/// Result of looking one song up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackOutcome {
    /// Lyrics were found (by this provider) and stored.
    Found(LyricsSource, LyricsQuality, LyricsProviderId),
    /// Existing lyrics were kept without a lookup.
    Kept(LyricsSource, LyricsQuality),
    /// No provider has lyrics.
    NotFound,
    /// These providers failed and should be asked again later.
    Deferred(Vec<LyricsProviderId>),
    /// The lookup failed.
    Failed(String),
}

/// Lyrics configuration applied live.
#[derive(Debug, Clone, Default)]
struct LyricsConfig {
    settings: LyricsSettings,
    overwrite: bool,
}

#[derive(Debug, Default)]
struct GateState {
    next_slot: Option<Instant>,
    cooldown_until: Option<Instant>,
}

/// Request spacing and cool-down of one provider.
#[derive(Debug, Default)]
struct Gate {
    state: Mutex<GateState>,
}

impl Gate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserves the next request slot (at least `spacing` after the
    /// previous one) and returns when it starts.
    fn reserve(&self, spacing: Duration) -> Instant {
        let now = Instant::now();
        let mut state = self.lock();
        let at = state.next_slot.map_or(now, |next| next.max(now));
        state.next_slot = Some(at + spacing);
        at
    }

    /// Time left until the provider may be asked again.
    fn cooldown_left(&self) -> Option<Duration> {
        let until = self.lock().cooldown_until?;
        let left = until.saturating_duration_since(Instant::now());
        (!left.is_zero()).then_some(left)
    }

    /// Holds requests back for at least `pause`.
    fn cool_down(&self, pause: Duration) {
        let until = Instant::now() + pause;
        let mut state = self.lock();
        state.cooldown_until = Some(state.cooldown_until.map_or(until, |u| u.max(until)));
    }
}

#[derive(Debug)]
struct Slot {
    provider: Arc<dyn LyricsProvider>,
    gate: Gate,
}

/// One provider's answer for one song.
enum Attempt {
    Hit(FetchedLyrics),
    Nothing,
    Deferred,
    Failed(String),
}

/// A downloaded song whose lyrics lookup is waiting for a retry.
#[derive(Debug, Clone)]
struct DeferredDownload {
    catalog_id: String,
    info: TrackInfo,
    providers: Vec<LyricsProviderId>,
    attempts: u8,
}

/// Lyrics lookups over the registered providers (see the module docs).
#[derive(Debug)]
pub(crate) struct LyricsEngine {
    storage: Storage,
    store: LyricsStore,
    slots: Vec<Slot>,
    config: RwLock<LyricsConfig>,
    deferred_downloads: Mutex<Vec<DeferredDownload>>,
    retrying: AtomicBool,
    downloads_idle: AtomicBool,
}

/// The stored source of lyrics from `id`. The database tells Apple's own
/// lyrics (never replaced) from fetched ones; every other provider is
/// stored under its own id.
fn source_of(id: LyricsProviderId) -> LyricsSource {
    match id {
        LyricsProviderId::Apple => LyricsSource::Apple,
        LyricsProviderId::Lrclib => LyricsSource::Lrclib,
        LyricsProviderId::Amll => LyricsSource::Amll,
        LyricsProviderId::Netease => LyricsSource::Netease,
        LyricsProviderId::Kugou => LyricsSource::Kugou,
        LyricsProviderId::Musixmatch => LyricsSource::Musixmatch,
        LyricsProviderId::Community => LyricsSource::Community,
    }
}

/// The lookup signature of a library track.
fn query_of(record: &TrackRecord) -> LyricsQuery {
    LyricsQuery {
        title: record.track.title.clone(),
        artist: record.track.artist.clone(),
        album: record.track.album.clone(),
        duration_ms: record.track.duration_ms,
        isrc: record.track.isrc.clone(),
    }
}

/// Wait before retry pass `pass` (1-based).
fn pass_delay(pass: u8) -> Duration {
    PASS_BASE * PASS_FACTOR.saturating_pow(u32::from(pass.saturating_sub(1)))
}

impl LyricsEngine {
    pub(crate) fn new(
        storage: Storage,
        store: LyricsStore,
        providers: Vec<Arc<dyn LyricsProvider>>,
    ) -> Self {
        Self {
            storage,
            store,
            slots: providers
                .into_iter()
                .map(|provider| Slot {
                    provider,
                    gate: Gate::default(),
                })
                .collect(),
            config: RwLock::new(LyricsConfig::default()),
            deferred_downloads: Mutex::new(Vec::new()),
            retrying: AtomicBool::new(false),
            downloads_idle: AtomicBool::new(true),
        }
    }

    pub(crate) fn store(&self) -> &LyricsStore {
        &self.store
    }

    /// Asks provider `id` once about `record`, without pacing, retries or
    /// storing anything (diagnostics). `None` when no such provider is
    /// registered.
    pub(crate) async fn probe(
        &self,
        id: LyricsProviderId,
        record: &TrackRecord,
    ) -> Option<Result<Option<FetchedLyrics>, ProviderError>> {
        let slot = self.slots.iter().find(|s| s.provider.id() == id)?;
        let query = query_of(record);
        let request = LyricsRequest {
            catalog_id: Some(record.catalog_or_id()),
            query: &query,
        };
        Some(slot.provider.lookup(&request).await)
    }

    /// Applies new settings; the next lookup uses them.
    pub(crate) fn configure(&self, settings: &LyricsSettings, overwrite: bool) {
        let mut config = self.config.write().unwrap_or_else(PoisonError::into_inner);
        *config = LyricsConfig {
            settings: settings.normalized(),
            overwrite,
        };
    }

    fn config(&self) -> LyricsConfig {
        self.config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn settings(&self) -> LyricsSettings {
        self.config().settings
    }

    /// Longest cool-down left among `providers`.
    fn cooldown_of(&self, providers: &[LyricsProviderId]) -> Duration {
        self.slots
            .iter()
            .filter(|slot| providers.contains(&slot.provider.id()))
            .filter_map(|slot| slot.gate.cooldown_left())
            .max()
            .unwrap_or_default()
    }

    /// Asks one provider, applying its failure policy.
    async fn ask(
        &self,
        slot: &Slot,
        request: &LyricsRequest<'_>,
        settings: &LyricsSettings,
        mode: LookupMode,
    ) -> Attempt {
        let id = slot.provider.id();
        let policy = match (mode, settings.policy_for(id)) {
            (LookupMode::Explicit, FailurePolicy::SkipAndRetryLater) => FailurePolicy::Skip,
            (_, policy) => policy,
        };
        let skipped = || match policy {
            FailurePolicy::SkipAndRetryLater => Attempt::Deferred,
            _ => Attempt::Failed(format!("{id} is not answering right now")),
        };
        let mut backoff = BACKOFF.iter();
        loop {
            if let Some(left) = slot.gate.cooldown_left() {
                if policy != FailurePolicy::WaitAndRetry && mode != LookupMode::Retry {
                    return skipped();
                }
                tracing::debug!(provider = %id, ?left, "lyrics provider cooling down; waiting");
                tokio::time::sleep(left).await;
            }
            tokio::time::sleep_until(slot.gate.reserve(settings.spacing(id))).await;
            match slot.provider.lookup(request).await {
                Ok(Some(lyrics)) => return Attempt::Hit(lyrics),
                Ok(None) => return Attempt::Nothing,
                Err(ProviderError::Fatal(reason)) => {
                    return Attempt::Failed(format!("{id}: {reason}"));
                }
                Err(ProviderError::Retryable {
                    reason,
                    retry_after,
                }) => {
                    let asked = retry_after.unwrap_or_default().min(MAX_WAIT);
                    // Without a `Retry-After`, hold the provider back briefly
                    // so a rate limit is not hammered by the next songs.
                    let pause = retry_after.map_or(IMPLICIT_COOLDOWN, |_| asked);
                    if policy == FailurePolicy::WaitAndRetry {
                        let Some(step) = backoff.next() else {
                            slot.gate.cool_down(pause);
                            return Attempt::Failed(format!("{id}: {reason}"));
                        };
                        tracing::debug!(provider = %id, %reason, "lyrics lookup failed; waiting");
                        slot.gate.cool_down((*step).max(asked));
                        continue;
                    }
                    tracing::debug!(provider = %id, %reason, "lyrics lookup failed; skipping");
                    slot.gate.cool_down(pause);
                    return skipped();
                }
            }
        }
    }

    /// Asks the enabled providers (restricted to `only`) in order and stores
    /// the best lyrics found under `lyrics_id`.
    async fn run_chain(
        &self,
        request: &LyricsRequest<'_>,
        lyrics_id: &str,
        only: Option<&[LyricsProviderId]>,
        mode: LookupMode,
        settings: &LyricsSettings,
    ) -> Result<TrackOutcome, LibraryError> {
        let target = settings.target_quality.max(LyricsQuality::Plain);
        let mut best: Option<(LyricsProviderId, FetchedLyrics)> = None;
        let mut deferred = Vec::new();
        let mut failure = None;
        for id in settings.enabled_providers() {
            if only.is_some_and(|only| !only.contains(&id)) {
                continue;
            }
            let Some(slot) = self.slots.iter().find(|s| s.provider.id() == id) else {
                continue;
            };
            let have = best
                .as_ref()
                .map_or(LyricsQuality::None, |(_, l)| l.quality);
            if have >= target {
                break;
            }
            if have >= slot.provider.best_quality() {
                continue;
            }
            match self.ask(slot, request, settings, mode).await {
                Attempt::Hit(lyrics) if lyrics.quality > have => best = Some((id, lyrics)),
                Attempt::Hit(_) | Attempt::Nothing => {}
                Attempt::Deferred => deferred.push(id),
                Attempt::Failed(reason) => failure = Some(reason),
            }
        }
        if let Some((id, lyrics)) = best {
            self.store
                .write(lyrics_id, lyrics.kind, &lyrics.ttml)
                .await?;
            return Ok(TrackOutcome::Found(source_of(id), lyrics.quality, id));
        }
        Ok(if !deferred.is_empty() {
            TrackOutcome::Deferred(deferred)
        } else if let Some(reason) = failure {
            TrackOutcome::Failed(reason)
        } else {
            TrackOutcome::NotFound
        })
    }

    /// `Kept(Custom)` when `lyrics_id` has custom lyrics, recording that
    /// state on `track_id`'s row (and every row of `lyrics_id`).
    async fn keep_custom(
        &self,
        lyrics_id: &str,
        track_id: Option<&str>,
    ) -> Result<Option<TrackOutcome>, LibraryError> {
        let Some(ttml) = self.store.read_custom(lyrics_id).await else {
            return Ok(None);
        };
        let quality = ttml_quality(&ttml);
        for id in std::iter::once(lyrics_id).chain(track_id.filter(|id| *id != lyrics_id)) {
            self.storage
                .set_lyrics_state(id, LyricsSource::Custom, quality)
                .await?;
        }
        Ok(Some(TrackOutcome::Kept(LyricsSource::Custom, quality)))
    }

    /// Looks one library track up.
    pub(crate) async fn lookup_record(
        &self,
        record: &TrackRecord,
        mode: LookupMode,
        only: Option<&[LyricsProviderId]>,
    ) -> Result<TrackOutcome, LibraryError> {
        let config = self.config();
        let lyrics_id = record.catalog_or_id();
        // The user's own lyrics always win: no lookup (explicit or bulk,
        // whatever "overwrite" says) ever replaces them.
        if let Some(outcome) = self.keep_custom(lyrics_id, Some(&record.track.id)).await? {
            return Ok(outcome);
        }
        // Apple's lyrics (word-timed when available) are the best there are:
        // they are never replaced.
        if record.track.lyrics_source == LyricsSource::Apple
            && let Some((ttml, _)) = self.store.read(lyrics_id, LyricsKind::Syllable).await
        {
            let quality = ttml_quality(&ttml);
            self.storage
                .set_lyrics_state(&record.track.id, LyricsSource::Apple, quality)
                .await?;
            return Ok(TrackOutcome::Kept(LyricsSource::Apple, quality));
        }
        // Word-timed files only ever come from Apple (saved before the track
        // row existed, so its state was never updated).
        if let Some(path) = self.store.path(lyrics_id, LyricsKind::Syllable)
            && let Ok(ttml) = tokio::fs::read_to_string(&path).await
            && !ttml.trim().is_empty()
        {
            let quality = ttml_quality(&ttml);
            self.storage
                .set_lyrics_state(&record.track.id, LyricsSource::Apple, quality)
                .await?;
            return Ok(TrackOutcome::Kept(LyricsSource::Apple, quality));
        }
        if mode != LookupMode::Explicit
            && !config.overwrite
            && let Some((ttml, _)) = self.store.read(lyrics_id, LyricsKind::Line).await
        {
            if record.track.lyrics_source != LyricsSource::None {
                return Ok(TrackOutcome::Kept(
                    record.track.lyrics_source,
                    record.track.lyrics_quality,
                ));
            }
            // A file the row does not know about (an earlier fetch or import).
            let quality = ttml_quality(&ttml);
            self.storage
                .set_lyrics_state(&record.track.id, LyricsSource::Lrclib, quality)
                .await?;
            return Ok(TrackOutcome::Kept(LyricsSource::Lrclib, quality));
        }
        let query = query_of(record);
        let request = LyricsRequest {
            catalog_id: Some(lyrics_id),
            query: &query,
        };
        let outcome = self
            .run_chain(&request, lyrics_id, only, mode, &config.settings)
            .await?;
        if let TrackOutcome::Found(source, quality, _) = outcome {
            self.storage
                .set_lyrics_state(&record.track.id, source, quality)
                .await?;
        }
        Ok(outcome)
    }

    /// Looks a downloaded song up: through its library row when there is
    /// one, otherwise by the queued `info`, stored under `catalog_id`.
    async fn lookup_download(
        &self,
        catalog_id: &str,
        info: &TrackInfo,
        only: Option<&[LyricsProviderId]>,
        mode: LookupMode,
    ) -> Result<TrackOutcome, LibraryError> {
        if let Some(record) = self.storage.find_track(catalog_id).await? {
            return self.lookup_record(&record, mode, only).await;
        }
        if let Some(outcome) = self.keep_custom(catalog_id, None).await? {
            return Ok(outcome);
        }
        let config = self.config();
        if !config.overwrite && self.store.exists(catalog_id).await {
            let quality = self
                .store
                .read(catalog_id, LyricsKind::Syllable)
                .await
                .map_or(LyricsQuality::None, |(ttml, _)| ttml_quality(&ttml));
            return Ok(TrackOutcome::Kept(LyricsSource::Lrclib, quality));
        }
        let query = LyricsQuery {
            title: info.title.clone(),
            artist: info.artist.clone(),
            album: info.album.clone(),
            duration_ms: info.duration_ms,
            isrc: None,
        };
        let request = LyricsRequest {
            catalog_id: Some(catalog_id),
            query: &query,
        };
        let outcome = self
            .run_chain(&request, catalog_id, only, mode, &config.settings)
            .await?;
        if let TrackOutcome::Found(source, quality, _) = outcome {
            // No-op when no row carries this id yet; a later catalog update
            // keeps the file, which the proxy serves by id.
            self.storage
                .set_lyrics_state(catalog_id, source, quality)
                .await?;
        }
        Ok(outcome)
    }

    /// Post-download lookup. Returns `None` when it was skipped (lyrics
    /// already present, or fetching after downloads is off).
    pub(crate) async fn after_download(
        self: &Arc<Self>,
        catalog_id: &str,
        info: &TrackInfo,
    ) -> Result<Option<TrackOutcome>, LibraryError> {
        if !self.config().settings.fetch_after_download {
            return Ok(None);
        }
        let outcome = self
            .lookup_download(catalog_id, info, None, LookupMode::Download)
            .await?;
        match &outcome {
            TrackOutcome::Kept(..) => return Ok(None),
            TrackOutcome::Deferred(providers) => {
                self.lock_deferred().push(DeferredDownload {
                    catalog_id: catalog_id.to_owned(),
                    info: info.clone(),
                    providers: providers.clone(),
                    attempts: 0,
                });
                self.maybe_retry_downloads();
            }
            _ => {}
        }
        Ok(Some(outcome))
    }

    fn lock_deferred(&self) -> std::sync::MutexGuard<'_, Vec<DeferredDownload>> {
        self.deferred_downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of downloaded songs waiting for a lyrics retry.
    pub(crate) fn deferred_download_count(&self) -> usize {
        self.lock_deferred().len()
    }

    /// The download queue became idle (`true`) or busy.
    pub(crate) fn set_downloads_idle(self: &Arc<Self>, idle: bool) {
        self.downloads_idle.store(idle, Ordering::Relaxed);
        if idle {
            self.maybe_retry_downloads();
        }
    }

    /// Starts the post-download retry task when the queue is idle and songs
    /// are waiting.
    fn maybe_retry_downloads(self: &Arc<Self>) {
        if !self.downloads_idle.load(Ordering::Relaxed)
            || self.lock_deferred().is_empty()
            || self.retrying.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine.retry_downloads().await;
            engine.retrying.store(false, Ordering::Release);
            engine.maybe_retry_downloads();
        });
    }

    /// Retry passes over the deferred post-download lookups while the queue
    /// stays idle.
    async fn retry_downloads(&self) {
        loop {
            let batch = std::mem::take(&mut *self.lock_deferred());
            let Some(attempt) = batch.iter().map(|d| d.attempts).min() else {
                return;
            };
            let providers: Vec<LyricsProviderId> =
                batch.iter().flat_map(|d| d.providers.clone()).collect();
            let wait = pass_delay(attempt + 1).max(self.cooldown_of(&providers));
            tracing::info!(
                songs = batch.len(),
                ?wait,
                "retrying deferred post-download lyrics"
            );
            tokio::time::sleep(wait).await;
            if !self.downloads_idle.load(Ordering::Relaxed) {
                // Downloads started again: wait for the next idle moment.
                self.lock_deferred().extend(batch);
                return;
            }
            let passes = self.config().settings.retry_passes;
            for mut item in batch {
                match self
                    .lookup_download(
                        &item.catalog_id,
                        &item.info,
                        Some(&item.providers),
                        LookupMode::Retry,
                    )
                    .await
                {
                    Ok(TrackOutcome::Deferred(providers)) => {
                        item.attempts += 1;
                        if item.attempts < passes {
                            item.providers = providers;
                            self.lock_deferred().push(item);
                        } else {
                            tracing::warn!(track = %item.catalog_id, "post-download lyrics: giving up after retries");
                        }
                    }
                    Ok(outcome) => {
                        tracing::debug!(track = %item.catalog_id, ?outcome, "post-download lyrics retried");
                    }
                    Err(error) => {
                        tracing::warn!(track = %item.catalog_id, %error, "post-download lyrics retry failed");
                    }
                }
            }
        }
    }

    /// Looks `track_ids` up one by one, then runs the retry passes over the
    /// songs skipped on a failure. `progress` sees every step.
    pub(crate) async fn run_batch(
        &self,
        track_ids: &[String],
        progress: &(dyn Fn(&LyricsProgress) + Send + Sync),
    ) -> LyricsReport {
        let mut report = LyricsReport::default();
        let mut state = LyricsProgress {
            total: track_ids.len(),
            pass_size: track_ids.len(),
            passes: self.config().settings.retry_passes,
            ..LyricsProgress::default()
        };
        progress(&state);
        let mut deferred: Vec<(String, Vec<LyricsProviderId>)> = Vec::new();
        for id in track_ids {
            let outcome = self.lookup_id(id, None, LookupMode::Bulk).await;
            Self::tally(&mut report, &mut state, &mut deferred, id, outcome, false);
            state.checked += 1;
            progress(&state);
        }
        let mut pass = 0u8;
        while !deferred.is_empty() {
            let passes = self.config().settings.retry_passes;
            state.passes = passes;
            if pass >= passes {
                break;
            }
            pass += 1;
            let providers: Vec<LyricsProviderId> =
                deferred.iter().flat_map(|(_, p)| p.clone()).collect();
            let wait = pass_delay(pass).max(self.cooldown_of(&providers));
            state.pass = pass;
            state.checked = 0;
            state.pass_size = deferred.len();
            state.waiting = Some(wait);
            progress(&state);
            tracing::info!(pass, songs = deferred.len(), ?wait, "lyrics retry pass");
            tokio::time::sleep(wait).await;
            state.waiting = None;
            report.passes = pass;
            for (id, only) in std::mem::take(&mut deferred) {
                state.deferred -= 1;
                let outcome = self.lookup_id(&id, Some(&only), LookupMode::Retry).await;
                Self::tally(&mut report, &mut state, &mut deferred, &id, outcome, true);
                state.checked += 1;
                progress(&state);
            }
        }
        report.failed += deferred.len();
        state.failed += deferred.len();
        state.deferred = 0;
        progress(&state);
        report
    }

    async fn lookup_id(
        &self,
        track_id: &str,
        only: Option<&[LyricsProviderId]>,
        mode: LookupMode,
    ) -> Result<TrackOutcome, LibraryError> {
        let record = self
            .storage
            .find_track(track_id)
            .await?
            .ok_or_else(|| LibraryError::UnknownTrack(track_id.to_owned()))?;
        self.lookup_record(&record, mode, only).await
    }

    fn tally(
        report: &mut LyricsReport,
        state: &mut LyricsProgress,
        deferred: &mut Vec<(String, Vec<LyricsProviderId>)>,
        id: &str,
        outcome: Result<TrackOutcome, LibraryError>,
        retry: bool,
    ) {
        match outcome {
            Ok(TrackOutcome::Found(_, quality, provider)) => {
                report.found += 1;
                state.found += 1;
                if retry {
                    report.recovered += 1;
                }
                match quality {
                    LyricsQuality::Word => report.word += 1,
                    LyricsQuality::Line => report.line += 1,
                    LyricsQuality::Plain | LyricsQuality::None => report.plain += 1,
                }
                report.by_provider[provider.index()] += 1;
            }
            Ok(TrackOutcome::Kept(..)) => {
                report.found += 1;
                report.kept += 1;
                state.found += 1;
            }
            Ok(TrackOutcome::NotFound) => {
                report.missing += 1;
                state.missing += 1;
            }
            Ok(TrackOutcome::Deferred(providers)) => {
                state.deferred += 1;
                deferred.push((id.to_owned(), providers));
            }
            Ok(TrackOutcome::Failed(reason)) => {
                report.failed += 1;
                state.failed += 1;
                tracing::warn!(track = %id, %reason, "lyrics lookup failed");
            }
            Err(error) => {
                report.failed += 1;
                state.failed += 1;
                tracing::warn!(track = %id, %error, "lyrics lookup failed");
            }
        }
    }
}

#[cfg(test)]
mod tests;
