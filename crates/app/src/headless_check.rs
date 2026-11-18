//! Headless checks against the real store, without a window or the proxy.
//!
//! ```text
//! cider-offline --download-check <catalog id>... [--concurrency N]
//! cider-offline --lyrics-check [--collection NAME] [--default-settings] [--overwrite]
//! cider-offline --lyrics-check --provider <id|all> [--collection NAME]... [--limit N] [--ids ID...] [--show-ttml]
//! ```
//!
//! - `--download-check` runs the given tracks through the real download queue
//!   and DRM downloader (the same code paths as the app) and prints every
//!   queue event with a timestamp: stages, progress, retries, the final
//!   status. Token values are never printed.
//! - `--lyrics-check` counts tracks with lyrics, runs "Fetch all missing
//!   lyrics" (or one collection's "Fetch lyrics"), and counts again.
//!   `--default-settings` uses the default lyrics settings instead of
//!   `config.toml`'s; `--overwrite` looks up songs that already have
//!   (non-Apple) lyrics too, like the "overwrite lyrics" setting.
//! - `--lyrics-check --provider <id>` asks one provider (or `all`, each in
//!   turn) about the chosen songs (`--ids`, or every `--collection`, spread
//!   down to `--limit` songs each) and prints the quality it found and what
//!   it matched (`--show-ttml`: and the start of the TTML). Nothing is stored.

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use cider_offline_core::download::DownloaderRegistry;
use cider_offline_core::storage::{LyricsQuality, LyricsSource};
use cider_offline_core::{
    AppPaths, Config, DownloadEvent, DownloadQueue, DownloadRecord, DownloadSourceKind,
    DownloadStatus, Library, LibraryFilter, LibraryQuery, LyricsProviderId, LyricsSettings,
    Storage,
};
use cider_offline_download::DrmDownloader;

/// Flag of the download check.
pub const DOWNLOAD_FLAG: &str = "--download-check";
/// Flag of the lyrics check.
pub const LYRICS_FLAG: &str = "--lyrics-check";
/// Give up on a download check after this long.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

/// Which check to run.
#[derive(Debug)]
pub enum Check {
    /// Download these catalog ids at this concurrency.
    Download {
        ids: Vec<String>,
        concurrency: usize,
    },
    /// Fetch missing lyrics (all, or one collection by name).
    Lyrics {
        collection: Option<String>,
        default_settings: bool,
        overwrite: bool,
    },
    /// Ask lyrics providers about songs, storing nothing.
    LyricsProbe {
        providers: Vec<LyricsProviderId>,
        collections: Vec<String>,
        ids: Vec<String>,
        limit: Option<usize>,
        show_ttml: bool,
    },
}

/// Values following `name` up to the next flag.
fn values_after(args: &[String], name: &str) -> Vec<String> {
    args.iter()
        .position(|a| a == name)
        .map(|at| {
            args[at + 1..]
                .iter()
                .take_while(|a| !a.starts_with("--"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Parses the command line; `None` when no check flag is present.
pub fn parse(args: &[String]) -> Option<Check> {
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if let Some(at) = args.iter().position(|a| a == DOWNLOAD_FLAG) {
        let ids = args[at + 1..]
            .iter()
            .take_while(|a| !a.starts_with("--"))
            .cloned()
            .collect();
        let concurrency = value("--concurrency")
            .and_then(|v| v.parse().ok())
            .unwrap_or(cider_offline_core::DEFAULT_CONCURRENCY);
        return Some(Check::Download { ids, concurrency });
    }
    if args.iter().any(|a| a == LYRICS_FLAG) {
        if let Some(provider) = value("--provider") {
            let providers = if provider == "all" {
                LyricsProviderId::ALL.to_vec()
            } else {
                LyricsProviderId::from_key(&provider).into_iter().collect()
            };
            let collections = args
                .iter()
                .enumerate()
                .filter(|(_, a)| *a == "--collection")
                .filter_map(|(i, _)| args.get(i + 1).cloned())
                .collect();
            return Some(Check::LyricsProbe {
                providers,
                collections,
                ids: values_after(args, "--ids"),
                limit: value("--limit").and_then(|v| v.parse().ok()),
                show_ttml: args.iter().any(|a| a == "--show-ttml"),
            });
        }
        return Some(Check::Lyrics {
            collection: value("--collection"),
            default_settings: args.iter().any(|a| a == "--default-settings"),
            overwrite: args.iter().any(|a| a == "--overwrite"),
        });
    }
    None
}

/// Runs a check and returns its process result.
pub fn run(check: Check) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating the tokio runtime")?;
    rt.block_on(async move {
        let paths = crate::startup::discover_paths()?;
        paths
            .ensure_dirs()
            .context("creating the data directories")?;
        let storage = Storage::open_async(paths.database_file()).await?;
        match check {
            Check::Download { ids, concurrency } => {
                download_check(storage, &paths, ids, concurrency).await
            }
            Check::Lyrics {
                collection,
                default_settings,
                overwrite,
            } => lyrics_check(storage, paths, collection, default_settings, overwrite).await,
            Check::LyricsProbe {
                providers,
                collections,
                ids,
                limit,
                show_ttml,
            } => {
                let songs = Songs {
                    collections,
                    ids,
                    limit,
                };
                lyrics_probe(storage, paths, &providers, songs, show_ttml).await
            }
        }
    })
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

async fn download_check(
    storage: Storage,
    paths: &AppPaths,
    ids: Vec<String>,
    concurrency: usize,
) -> anyhow::Result<()> {
    if ids.is_empty() {
        bail!("give at least one catalog id after {DOWNLOAD_FLAG}");
    }
    let wvd = paths.device_override_file();
    let drm = if wvd.exists() {
        let bytes = tokio::fs::read(&wvd).await?;
        DrmDownloader::from_wvd_bytes(storage.clone(), &bytes)
    } else {
        DrmDownloader::with_default_device(storage.clone())
    }
    .map_err(|e| anyhow::anyhow!("DRM downloader unavailable: {e}"))?;
    let mut registry = DownloaderRegistry::new();
    registry.register(drm);
    let queue = DownloadQueue::spawn(
        storage.clone(),
        Arc::new(registry),
        paths.media_dir(),
        Arc::new(AtomicU64::new(0)),
        None,
    );
    queue.set_concurrency(concurrency);
    let mut events = queue.events();

    let mut records = Vec::with_capacity(ids.len());
    for id in &ids {
        // A previous run's row would be kept as-is; start each check fresh.
        storage.delete_download(id).await?;
        let (title, artist, album) = match storage.find_track(id).await? {
            Some(found) => (found.track.title, found.track.artist, found.track.album),
            None => (id.clone(), String::new(), None),
        };
        records.push(DownloadRecord {
            id: id.clone(),
            track_id: id.clone(),
            title,
            artist,
            album,
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
        });
    }
    say(&format!(
        "download-check: {} track(s), concurrency {concurrency}",
        ids.len()
    ));
    queue.enqueue(records)?;

    let outcome = watch(&mut events, &ids).await?;
    say("RESULTS:");
    for (id, result) in &outcome {
        say(&format!("  {id}: {result}"));
    }
    Ok(())
}

/// Prints every queue event until each of `ids` reached a final state, and
/// returns `(id, result)` in completion order.
async fn watch(
    events: &mut tokio::sync::broadcast::Receiver<DownloadEvent>,
    ids: &[String],
) -> anyhow::Result<Vec<(String, String)>> {
    let start = Instant::now();
    let mut pending: HashSet<String> = ids.iter().cloned().collect();
    let mut outcome: Vec<(String, String)> = Vec::new();
    while !pending.is_empty() {
        let event = tokio::time::timeout(DOWNLOAD_TIMEOUT, events.recv())
            .await
            .context("timed out waiting for the queue")?;
        let event = match event {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                say(&format!("  (lagged {n} events)"));
                continue;
            }
            Err(error) => bail!("queue closed: {error}"),
        };
        let at = start.elapsed().as_secs_f32();
        match event {
            DownloadEvent::Added(record) => {
                say(&format!(
                    "{at:7.2}s {} added as {:?}",
                    record.id, record.status
                ));
                if record.status == DownloadStatus::Unavailable && pending.remove(&record.id) {
                    let reason = record.message.unwrap_or_default();
                    outcome.push((record.id, format!("Can't download yet ({reason})")));
                }
            }
            DownloadEvent::StatusChanged {
                id,
                status,
                progress,
                message,
                stage,
                attempt,
            } => {
                let stage = stage.map_or("-", |s| s.label());
                let note = message.as_deref().unwrap_or("");
                say(&format!(
                    "{at:7.2}s {id} {status:?} stage={stage} progress={progress:.2} attempt={attempt} {note}"
                ));
                let terminal = match status {
                    DownloadStatus::Done => Some("Done".to_owned()),
                    DownloadStatus::Failed => Some(format!("Failed ({note})")),
                    DownloadStatus::Unavailable => Some(format!("Can't download yet ({note})")),
                    _ => None,
                };
                if let Some(terminal) = terminal
                    && pending.remove(&id)
                {
                    outcome.push((id, terminal));
                }
            }
            DownloadEvent::Removed(_)
            | DownloadEvent::AudioRemoved(_)
            | DownloadEvent::SummaryChanged { .. } => {}
        }
    }
    Ok(outcome)
}

/// Lyrics counts by source and quality.
#[derive(Debug, Default)]
struct LyricsCounts {
    total: usize,
    with: usize,
    apple: usize,
    fetched: usize,
    word: usize,
    line: usize,
    plain: usize,
}

impl LyricsCounts {
    async fn of(storage: &Storage) -> anyhow::Result<Self> {
        let mut counts = Self::default();
        for track in storage.list_tracks().await? {
            counts.total += 1;
            match track.lyrics_source {
                LyricsSource::None => continue,
                LyricsSource::Apple => counts.apple += 1,
                LyricsSource::Lrclib
                | LyricsSource::Amll
                | LyricsSource::Netease
                | LyricsSource::Kugou
                | LyricsSource::Musixmatch
                | LyricsSource::Community
                | LyricsSource::Custom => counts.fetched += 1,
            }
            counts.with += 1;
            match track.lyrics_quality {
                LyricsQuality::Word => counts.word += 1,
                LyricsQuality::Line => counts.line += 1,
                LyricsQuality::Plain | LyricsQuality::None => counts.plain += 1,
            }
        }
        Ok(counts)
    }

    fn line(&self, when: &str) -> String {
        format!(
            "lyrics {when}: {} of {} tracks have lyrics (Apple {}, other providers {}; word-synced {}, line-synced {}, plain {})",
            self.with, self.total, self.apple, self.fetched, self.word, self.line, self.plain
        )
    }
}

/// Track ids of the Apple Music collection called `name`.
async fn collection_ids(storage: &Storage, name: &str) -> anyhow::Result<Vec<String>> {
    let query = LibraryQuery {
        filter: LibraryFilter::AppleMusic,
        ..LibraryQuery::default()
    };
    let all = storage.collections_page(query, 0, usize::MAX).await?;
    let Some(entry) = all
        .items
        .into_iter()
        .find(|c| c.collection.name.trim() == name)
    else {
        bail!("collection {name:?} not found");
    };
    Ok(storage
        .collection_members(LibraryFilter::AppleMusic, &entry.collection.id)
        .await?
        .unwrap_or_default())
}

/// Up to `limit` items spread evenly over `items`.
fn spread<T: Clone>(items: &[T], limit: Option<usize>) -> Vec<T> {
    match limit {
        Some(limit) if limit > 0 && items.len() > limit => (0..limit)
            .filter_map(|i| items.get(i * items.len() / limit).cloned())
            .collect(),
        _ => items.to_vec(),
    }
}

/// Per-provider tally of a probe.
#[derive(Debug, Default, Clone, Copy)]
struct ProbeTally {
    word: usize,
    line: usize,
    plain: usize,
    none: usize,
    failed: usize,
}

/// Songs chosen for a probe.
struct Songs {
    collections: Vec<String>,
    ids: Vec<String>,
    limit: Option<usize>,
}

async fn lyrics_probe(
    storage: Storage,
    paths: AppPaths,
    providers: &[LyricsProviderId],
    songs: Songs,
    show_ttml: bool,
) -> anyhow::Result<()> {
    let Songs {
        collections,
        mut ids,
        limit,
    } = songs;
    if providers.is_empty() {
        bail!(
            "unknown provider; use one of: all, {}",
            LyricsProviderId::ALL.map(LyricsProviderId::key).join(", ")
        );
    }
    for name in &collections {
        ids.extend(spread(&collection_ids(&storage, name).await?, limit));
    }
    if ids.is_empty() {
        bail!("choose songs with --ids or --collection");
    }
    let library = Library::new(storage.clone(), paths)?;
    let mut tallies = Vec::new();
    for &provider in providers {
        say(&format!("== {} ({}) ==", provider.label(), provider.key()));
        let mut tally = ProbeTally::default();
        for id in &ids {
            let Some(record) = storage.find_track(id).await? else {
                say(&format!("  {id}: not in the library"));
                continue;
            };
            let track = &record.track;
            let song = format!(
                "{} - {} ({} s)",
                track.artist,
                track.title,
                track.duration_ms.unwrap_or(0) / 1_000
            );
            let started = Instant::now();
            let answer = library.probe_lyrics(provider, id).await?;
            let took = started.elapsed().as_millis();
            let line = match answer {
                Ok(Some(found)) => {
                    match found.quality {
                        LyricsQuality::Word => tally.word += 1,
                        LyricsQuality::Line => tally.line += 1,
                        LyricsQuality::Plain | LyricsQuality::None => tally.plain += 1,
                    }
                    let mut text = format!(
                        "{:?} [{}]",
                        found.quality,
                        found.matched.as_deref().unwrap_or("by id")
                    );
                    if show_ttml {
                        let head: String = found.ttml.chars().take(900).collect();
                        text.push_str(
                            "
    ",
                        );
                        text.push_str(&head);
                    }
                    text
                }
                Ok(None) => {
                    tally.none += 1;
                    "none".to_owned()
                }
                Err(error) => {
                    tally.failed += 1;
                    let kind = if error.is_retryable() {
                        "retryable"
                    } else {
                        "fatal"
                    };
                    format!("FAILED ({kind}): {error}")
                }
            };
            say(&format!("  {song}: {line} ({took} ms)"));
            tokio::time::sleep(provider.default_spacing()).await;
        }
        tallies.push((provider, tally));
    }
    say("SUMMARY (songs: found / word / line / plain / none / failed):");
    for (provider, t) in tallies {
        say(&format!(
            "  {:<20} {:>3} / {:>3} / {:>3} / {:>3} / {:>3} / {:>3}",
            provider.label(),
            t.word + t.line + t.plain,
            t.word,
            t.line,
            t.plain,
            t.none,
            t.failed
        ));
    }
    Ok(())
}

async fn lyrics_check(
    storage: Storage,
    paths: AppPaths,
    collection: Option<String>,
    default_settings: bool,
    overwrite: bool,
) -> anyhow::Result<()> {
    // The user's settings (config.toml), as the app would apply them.
    let mut config =
        Config::load_or_default(&paths.config_file()).context("reading config.toml")?;
    if default_settings {
        config.lyrics = LyricsSettings::default();
    }
    if overwrite {
        config.overwrite_lyrics = true;
    }
    let library = Library::new(storage.clone(), paths)?;
    library.apply_lyrics_config(&config);
    let settings = library.lyrics_settings();
    let order: Vec<String> = settings
        .providers
        .iter()
        .map(|p| format!("{}{}", p.id.label(), if p.enabled { "" } else { " (off)" }))
        .collect();
    say(&format!(
        "settings: providers [{}], on failure {:?} for [{}], search until {:?}, LRCLIB spacing {} ms, {} retry passes, overwrite {}",
        order.join(" > "),
        settings.on_failure,
        settings
            .policy_providers
            .iter()
            .map(|p| p.label())
            .collect::<Vec<_>>()
            .join(", "),
        settings.target_quality,
        settings.lrclib_spacing_ms,
        settings.retry_passes,
        config.overwrite_lyrics,
    ));
    say(&LyricsCounts::of(&storage).await?.line("before"));
    let ids: Vec<String> = match &collection {
        None => storage
            .tracks_with_lyrics_source(LyricsSource::None)
            .await?
            .into_iter()
            .map(|record| record.track.id)
            .collect(),
        Some(name) => collection_ids(&storage, name).await?,
    };
    say(&format!(
        "looking up {} songs ({})",
        ids.len(),
        collection.as_deref().unwrap_or("all missing lyrics")
    ));
    let started = Instant::now();
    let last_pass = std::sync::Mutex::new((u8::MAX, None::<Duration>));
    let report = library
        .fetch_lyrics_for_with(&ids, &|p| {
            let t = started.elapsed().as_secs_f32();
            let mut last = last_pass.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let pass_changed = last.0 != p.pass || last.1.is_some() != p.waiting.is_some();
            let every = p.checked > 0 && p.checked % 25 == 0;
            let end = p.checked == p.pass_size;
            if pass_changed || every || end {
                *last = (p.pass, p.waiting);
                let phase = match (p.pass, p.waiting) {
                    (0, _) => "batch".to_owned(),
                    (pass, Some(wait)) => {
                        format!("waiting {} s before pass {pass} of {}", wait.as_secs(), p.passes)
                    }
                    (pass, None) => format!("retry pass {pass} of {}", p.passes),
                };
                say(&format!(
                    "  [{t:7.1}s] {phase}: {}/{} checked, found {}, none {}, deferred {}, failed {}",
                    p.checked, p.pass_size, p.found, p.missing, p.deferred, p.failed
                ));
            }
        })
        .await;
    say(&format!(
        "result: found {} (kept {}; new: word-synced {}, line-synced {}, plain {}; from {}), none {}, failed {}, recovered by retries {}, retry passes {}, took {:.0}s",
        report.found,
        report.kept,
        report.word,
        report.line,
        report.plain,
        LyricsProviderId::ALL
            .iter()
            .map(|id| format!("{} {}", id.label(), report.from(*id)))
            .collect::<Vec<_>>()
            .join(", "),
        report.missing,
        report.failed,
        report.recovered,
        report.passes,
        started.elapsed().as_secs_f32()
    ));
    say(&LyricsCounts::of(&storage).await?.line("after"));
    Ok(())
}
