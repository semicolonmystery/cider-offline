//! Engine tests with scripted providers on a paused clock.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::config::AppPaths;
use crate::download::{DownloaderRegistry, TrackInfo};
use crate::library::{Library, LibraryError};
use crate::lyrics::{
    FailurePolicy, FetchedLyrics, LrclibClient, LyricsKind, LyricsProvider, LyricsProviderId,
    LyricsQuality, LyricsRequest, LyricsSettings, ProviderError, ProviderSetting,
};
use crate::storage::{CatalogUpdate, LyricsSource, Storage, TrackMetadata};

/// One scripted answer.
#[derive(Debug, Clone, Copy)]
enum Reply {
    Found(LyricsQuality),
    Nothing,
    /// HTTP 429 with this `Retry-After` (`None`: header missing).
    RateLimited(Option<Duration>),
}

const NO_WAIT: Option<Duration> = Some(Duration::ZERO);

/// Provider answering from a per-title script (the last reply repeats) and
/// logging the titles it was asked for.
#[derive(Debug)]
struct Scripted {
    id: LyricsProviderId,
    best: LyricsQuality,
    script: Mutex<HashMap<String, VecDeque<Reply>>>,
    calls: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(id: LyricsProviderId, best: LyricsQuality, script: &[(&str, &[Reply])]) -> Arc<Self> {
        Arc::new(Self {
            id,
            best,
            script: Mutex::new(
                script
                    .iter()
                    .map(|(title, replies)| {
                        ((*title).to_owned(), replies.iter().copied().collect())
                    })
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn ttml(quality: LyricsQuality) -> String {
    match quality {
        LyricsQuality::Word => r#"<tt itunes:timing="Word"><body><p begin="0"><span begin="0">a</span></p></body></tt>"#,
        LyricsQuality::Line => r#"<tt itunes:timing="Line"><body><p begin="0">a</p></body></tt>"#,
        _ => r#"<tt itunes:timing="None"><body><p>a</p></body></tt>"#,
    }
    .to_owned()
}

#[async_trait]
impl LyricsProvider for Scripted {
    fn id(&self) -> LyricsProviderId {
        self.id
    }

    fn best_quality(&self) -> LyricsQuality {
        self.best
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let title = request.query.title.clone();
        self.calls.lock().unwrap().push(title.clone());
        let reply = {
            let mut script = self.script.lock().unwrap();
            let queue = script.entry(title).or_default();
            if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().copied()
            }
        };
        match reply.unwrap_or(Reply::Nothing) {
            Reply::Found(quality) => Ok(Some(FetchedLyrics {
                ttml: ttml(quality),
                kind: if quality == LyricsQuality::Word {
                    LyricsKind::Syllable
                } else {
                    LyricsKind::Line
                },
                quality,
                matched: None,
            })),
            Reply::Nothing => Ok(None),
            Reply::RateLimited(retry_after) => Err(ProviderError::Retryable {
                reason: "rate limited (HTTP 429)".to_owned(),
                retry_after,
            }),
        }
    }
}

fn lrclib(script: &[(&str, &[Reply])]) -> Arc<Scripted> {
    Scripted::new(LyricsProviderId::Lrclib, LyricsQuality::Line, script)
}

fn apple(script: &[(&str, &[Reply])]) -> Arc<Scripted> {
    Scripted::new(LyricsProviderId::Apple, LyricsQuality::Word, script)
}

fn order(ids: &[(LyricsProviderId, bool)]) -> Vec<ProviderSetting> {
    ids.iter()
        .map(|&(id, enabled)| ProviderSetting { id, enabled })
        .collect()
}

struct Fixture {
    _dir: tempfile::TempDir,
    library: Library,
    ids: Vec<String>,
}

/// Library with songs `s1..=s{count}` (ids `i.N`, catalog ids `N`).
async fn fixture(
    count: usize,
    providers: Vec<Arc<dyn LyricsProvider>>,
    settings: &LyricsSettings,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::under(dir.path());
    paths.ensure_dirs().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let tracks = (1..=count)
        .map(|n| TrackMetadata {
            id: format!("i.{n}"),
            catalog_id: Some(n.to_string()),
            title: format!("s{n}"),
            artist: "Artist".to_owned(),
            duration_ms: Some(200_000),
            ..TrackMetadata::default()
        })
        .collect();
    storage
        .apply_catalog_update(CatalogUpdate {
            tracks,
            ..CatalogUpdate::default()
        })
        .await
        .unwrap();
    let library = Library::with_options(
        storage,
        paths,
        LrclibClient::new("http://127.0.0.1:9").unwrap(),
        DownloaderRegistry::new(),
    )
    .unwrap()
    .with_lyrics_providers(providers);
    library.apply_lyrics_settings(settings, false);
    Fixture {
        _dir: dir,
        library,
        ids: (1..=count).map(|n| format!("i.{n}")).collect(),
    }
}

fn lrclib_only(policy: FailurePolicy) -> LyricsSettings {
    LyricsSettings {
        providers: order(&[
            (LyricsProviderId::Lrclib, true),
            (LyricsProviderId::Apple, false),
        ]),
        on_failure: policy,
        ..LyricsSettings::default()
    }
}

const FOUND_LINE: &[Reply] = &[Reply::Found(LyricsQuality::Line)];

#[tokio::test(start_paused = true)]
async fn wait_policy_should_retry_the_same_song_before_moving_on() {
    let provider = lrclib(&[
        (
            "s1",
            &[
                Reply::RateLimited(NO_WAIT),
                Reply::Found(LyricsQuality::Line),
            ],
        ),
        ("s2", FOUND_LINE),
    ]);
    let f = fixture(
        2,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::WaitAndRetry),
    )
    .await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(
        (provider.calls(), report.found),
        (vec!["s1".into(), "s1".into(), "s2".into()], 2)
    );
}

#[tokio::test(start_paused = true)]
async fn skip_and_retry_should_continue_and_retry_after_the_batch() {
    let provider = lrclib(&[
        (
            "s1",
            &[
                Reply::RateLimited(NO_WAIT),
                Reply::Found(LyricsQuality::Line),
            ],
        ),
        ("s2", FOUND_LINE),
        ("s3", FOUND_LINE),
    ]);
    let f = fixture(
        3,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(provider.calls(), vec!["s1", "s2", "s3", "s1"]);
    assert_eq!(
        (report.found, report.recovered, report.passes, report.failed),
        (3, 1, 1, 0)
    );
}

#[tokio::test(start_paused = true)]
async fn skip_and_retry_should_respect_max_passes() {
    let provider = lrclib(&[("s1", &[Reply::RateLimited(NO_WAIT)])]);
    let settings = LyricsSettings {
        retry_passes: 2,
        ..lrclib_only(FailurePolicy::SkipAndRetryLater)
    };
    let f = fixture(1, vec![provider.clone()], &settings).await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(
        (provider.calls().len(), report.passes, report.failed),
        (3, 2, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn skip_policy_should_not_retry() {
    let provider = lrclib(&[("s1", &[Reply::RateLimited(NO_WAIT)]), ("s2", FOUND_LINE)]);
    let f = fixture(2, vec![provider.clone()], &lrclib_only(FailurePolicy::Skip)).await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(provider.calls(), vec!["s1", "s2"]);
    assert_eq!((report.found, report.failed, report.passes), (1, 1, 0));
}

#[tokio::test(start_paused = true)]
async fn not_found_should_never_be_retried() {
    let provider = lrclib(&[("s1", &[Reply::Nothing])]);
    let f = fixture(
        1,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(
        (provider.calls().len(), report.missing, report.passes),
        (1, 1, 0)
    );
}

#[tokio::test(start_paused = true)]
async fn policy_should_apply_only_to_selected_providers() {
    // LRCLIB is skipped and retried at the end; Apple (not selected) waits.
    let apple = apple(&[
        ("s1", &[Reply::RateLimited(NO_WAIT), Reply::Nothing]),
        ("s2", &[Reply::Nothing]),
    ]);
    let lrclib = lrclib(&[
        (
            "s1",
            &[
                Reply::RateLimited(NO_WAIT),
                Reply::Found(LyricsQuality::Line),
            ],
        ),
        ("s2", FOUND_LINE),
    ]);
    let settings = LyricsSettings {
        on_failure: FailurePolicy::SkipAndRetryLater,
        policy_providers: vec![LyricsProviderId::Lrclib],
        ..LyricsSettings::default()
    };
    let f = fixture(2, vec![apple.clone(), lrclib.clone()], &settings).await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(apple.calls(), vec!["s1", "s1", "s2"]);
    assert_eq!(lrclib.calls(), vec!["s1", "s2", "s1"]);
    assert_eq!(report.found, 2);
}

#[tokio::test(start_paused = true)]
async fn rate_limit_without_retry_after_should_skip_songs_during_cooldown() {
    let provider = lrclib(&[
        (
            "s1",
            &[Reply::RateLimited(None), Reply::Found(LyricsQuality::Line)],
        ),
        ("s2", FOUND_LINE),
    ]);
    let f = fixture(
        2,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    // s2 is deferred without a request while LRCLIB cools down.
    assert_eq!(provider.calls(), vec!["s1", "s1", "s2"]);
    assert_eq!(report.found, 2);
}

#[tokio::test(start_paused = true)]
async fn retry_pass_should_honour_retry_after() {
    let provider = lrclib(&[(
        "s1",
        &[
            Reply::RateLimited(Some(Duration::from_mins(1))),
            Reply::Found(LyricsQuality::Line),
        ],
    )]);
    let f = fixture(
        1,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    let started = Instant::now();
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert!(report.found == 1 && started.elapsed() >= Duration::from_mins(1));
}

#[tokio::test(start_paused = true)]
async fn progress_should_report_deferred_songs_and_the_retry_pass() {
    let provider = lrclib(&[
        (
            "s1",
            &[
                Reply::RateLimited(NO_WAIT),
                Reply::Found(LyricsQuality::Line),
            ],
        ),
        ("s2", FOUND_LINE),
    ]);
    let f = fixture(
        2,
        vec![provider],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    let seen = Mutex::new(Vec::new());
    f.library
        .fetch_lyrics_for_with(&f.ids, &|p| seen.lock().unwrap().push(*p))
        .await;
    let seen = seen.into_inner().unwrap();
    assert!(
        seen.iter()
            .any(|p| p.pass == 0 && p.deferred == 1 && p.checked == 2)
            && seen
                .iter()
                .any(|p| p.pass == 1 && p.passes == 3 && p.waiting.is_some())
    );
}

#[tokio::test(start_paused = true)]
async fn disabled_provider_should_never_be_asked() {
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Word)])]);
    let lrclib = lrclib(&[("s1", FOUND_LINE)]);
    let settings = LyricsSettings {
        providers: order(&[
            (LyricsProviderId::Apple, false),
            (LyricsProviderId::Lrclib, true),
        ]),
        ..LyricsSettings::default()
    };
    let f = fixture(1, vec![apple.clone(), lrclib], &settings).await;
    f.library.fetch_lyrics_for(&f.ids).await;
    assert!(apple.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn order_should_decide_who_is_asked_first() {
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Line)])]);
    let lrclib = lrclib(&[("s1", FOUND_LINE)]);
    let settings = LyricsSettings {
        providers: order(&[
            (LyricsProviderId::Lrclib, true),
            (LyricsProviderId::Apple, true),
        ]),
        target_quality: LyricsQuality::Line,
        ..LyricsSettings::default()
    };
    let f = fixture(1, vec![apple.clone(), lrclib.clone()], &settings).await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!(
        (
            lrclib.calls().len(),
            apple.calls().len(),
            report.from(LyricsProviderId::Lrclib)
        ),
        (1, 0, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn best_quality_should_win_over_priority() {
    // LRCLIB comes first but only has plain lyrics; Apple has word-synced.
    let lrclib = lrclib(&[("s1", &[Reply::Found(LyricsQuality::Plain)])]);
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Word)])]);
    let settings = LyricsSettings {
        providers: order(&[
            (LyricsProviderId::Lrclib, true),
            (LyricsProviderId::Apple, true),
        ]),
        ..LyricsSettings::default()
    };
    let f = fixture(1, vec![apple, lrclib], &settings).await;
    f.library.fetch_lyrics_for(&f.ids).await;
    let track = f.library.storage().get_track("i.1").await.unwrap().unwrap();
    assert_eq!(
        (track.lyrics_source, track.lyrics_quality),
        (LyricsSource::Apple, LyricsQuality::Word)
    );
}

#[tokio::test(start_paused = true)]
async fn word_synced_result_should_stop_the_search() {
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Word)])]);
    let lrclib = lrclib(&[("s1", FOUND_LINE)]);
    let f = fixture(1, vec![apple, lrclib.clone()], &LyricsSettings::default()).await;
    f.library.fetch_lyrics_for(&f.ids).await;
    assert!(lrclib.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn provider_that_cannot_improve_should_be_skipped() {
    // Apple found line-synced; LRCLIB can never beat that.
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Line)])]);
    let lrclib = lrclib(&[("s1", FOUND_LINE)]);
    let f = fixture(1, vec![apple, lrclib.clone()], &LyricsSettings::default()).await;
    f.library.fetch_lyrics_for(&f.ids).await;
    assert!(lrclib.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn plain_lyrics_should_keep_looking_for_synced_ones() {
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Plain)])]);
    let lrclib = lrclib(&[("s1", FOUND_LINE)]);
    let f = fixture(1, vec![apple, lrclib], &LyricsSettings::default()).await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!((report.line, report.from(LyricsProviderId::Lrclib)), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn any_quality_target_should_stop_at_first_lyrics() {
    let apple = apple(&[("s1", &[Reply::Found(LyricsQuality::Plain)])]);
    let lrclib = lrclib(&[("s1", FOUND_LINE)]);
    let settings = LyricsSettings {
        target_quality: LyricsQuality::Plain,
        ..LyricsSettings::default()
    };
    let f = fixture(1, vec![apple, lrclib.clone()], &settings).await;
    f.library.fetch_lyrics_for(&f.ids).await;
    assert!(lrclib.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn bulk_fetch_should_keep_existing_lyrics_without_overwrite() {
    let provider = lrclib(&[("s1", FOUND_LINE)]);
    let f = fixture(1, vec![provider.clone()], &lrclib_only(FailurePolicy::Skip)).await;
    f.library.fetch_lyrics_for(&f.ids).await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    assert_eq!((provider.calls().len(), report.kept), (1, 1));
}

fn info(title: &str) -> TrackInfo {
    TrackInfo {
        title: title.to_owned(),
        artist: "Artist".to_owned(),
        album: None,
        duration_ms: Some(200_000),
    }
}

#[tokio::test(start_paused = true)]
async fn failed_download_lookup_should_be_retried_when_queue_is_idle() {
    let provider = lrclib(&[(
        "s1",
        &[
            Reply::RateLimited(NO_WAIT),
            Reply::Found(LyricsQuality::Line),
        ],
    )]);
    let f = fixture(
        1,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    f.library.set_downloads_idle(false);
    let first = f.library.ensure_download_lyrics("1", &info("s1")).await;
    assert!(matches!(first, Err(LibraryError::LyricsDeferred { .. })));
    assert_eq!(f.library.deferred_download_lyrics(), 1);
    f.library.set_downloads_idle(true);
    tokio::time::sleep(Duration::from_mins(1)).await;
    let track = f.library.storage().get_track("i.1").await.unwrap().unwrap();
    assert_eq!(
        (f.library.deferred_download_lyrics(), track.lyrics_source),
        (0, LyricsSource::Lrclib)
    );
}

#[tokio::test(start_paused = true)]
async fn download_lookup_should_wait_while_queue_is_busy() {
    let provider = lrclib(&[(
        "s1",
        &[
            Reply::RateLimited(NO_WAIT),
            Reply::Found(LyricsQuality::Line),
        ],
    )]);
    let f = fixture(
        1,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    f.library.set_downloads_idle(false);
    let _ = f.library.ensure_download_lyrics("1", &info("s1")).await;
    tokio::time::sleep(Duration::from_mins(2)).await;
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn lyrics_after_download_can_be_turned_off() {
    let provider = lrclib(&[("s1", FOUND_LINE)]);
    let settings = LyricsSettings {
        fetch_after_download: false,
        ..lrclib_only(FailurePolicy::Skip)
    };
    let f = fixture(1, vec![provider.clone()], &settings).await;
    let outcome = f
        .library
        .ensure_download_lyrics("1", &info("s1"))
        .await
        .unwrap();
    assert!(outcome.is_none() && provider.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn retry_pass_should_wait_for_cooldown_instead_of_skipping() {
    let provider = lrclib(&[
        (
            "s1",
            &[
                Reply::RateLimited(None),
                Reply::RateLimited(None),
                Reply::Found(LyricsQuality::Line),
            ],
        ),
        ("s2", FOUND_LINE),
    ]);
    let f = fixture(
        2,
        vec![provider.clone()],
        &lrclib_only(FailurePolicy::SkipAndRetryLater),
    )
    .await;
    let report = f.library.fetch_lyrics_for(&f.ids).await;
    // Pass 1: s1 fails again, s2 is asked once the cool-down is over.
    assert_eq!(provider.calls(), vec!["s1", "s1", "s2", "s1"]);
    assert_eq!(report.found, 2);
}
