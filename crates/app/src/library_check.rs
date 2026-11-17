//! Headless check of the Apple Music library sync (`--library-check`).
//!
//! Runs one sync against the live Apple Music API with the stored tokens,
//! then reads the Apple Music library view exactly as the GUI does and
//! verifies it against what Apple returned. No window, no proxy.
//!
//! ```text
//! cider-offline --library-check [--fresh] [--import-tokens <store.sqlite>]
//! ```
//!
//! - default: syncs into the app's real store.
//! - `--fresh`: syncs into a brand-new temporary store that holds nothing but
//!   the auth tokens copied from the real store, proving the library loads
//!   completely without any passively captured Cider traffic.
//! - `--import-tokens <path>`: copies the auth tokens from another store file
//!   into the target store first (used after a schema reset). Token values are
//!   never printed.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use cider_offline_core::hyper::Uri;
use cider_offline_core::storage::CollectionKind;
use cider_offline_core::{
    AppPaths, CancellationToken, Library, LibraryFilter, LibraryQuery, LibrarySnapshot,
    LibrarySync, Storage, artwork,
};

/// The flag that selects this mode.
pub const FLAG: &str = "--library-check";

/// Options parsed from the command line.
#[derive(Debug, Default)]
pub struct Options {
    fresh: bool,
    import_tokens: Option<PathBuf>,
}

/// Returns the options when `--library-check` is present.
pub fn parse(args: &[String]) -> Option<Options> {
    if !args.iter().any(|a| a == FLAG) {
        return None;
    }
    let mut options = Options::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--fresh" => options.fresh = true,
            "--import-tokens" => options.import_tokens = iter.next().map(PathBuf::from),
            _ => {}
        }
    }
    Some(options)
}

/// Runs the check; `Ok(true)` when every assertion passed.
pub fn run(options: &Options) -> anyhow::Result<bool> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating the tokio runtime")?;
    rt.block_on(check(options))
}

/// Accumulates output lines and failures.
struct Report {
    out: std::io::Stdout,
    failures: Vec<String>,
}

impl Report {
    fn line(&mut self, text: &str) {
        // Best effort: a closed stdout must not abort the check.
        let _ = writeln!(self.out.lock(), "{text}");
    }

    fn fail(&mut self, text: String) {
        self.line(&format!("  FAIL: {text}"));
        self.failures.push(text);
    }
}

async fn check(options: &Options) -> anyhow::Result<bool> {
    let real = crate::startup::discover_paths()?;
    let mut report = Report {
        out: std::io::stdout(),
        failures: Vec::new(),
    };

    let (paths, _temp) = if options.fresh {
        let dir = std::env::temp_dir().join(format!("cider-library-check-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).context("clearing the temporary store")?;
        }
        let paths = AppPaths::under(&dir);
        (paths, Some(TempDir(dir)))
    } else {
        (real.clone(), None)
    };
    paths.ensure_dirs().context("creating data directories")?;
    let storage = Storage::open_async(paths.database_file())
        .await
        .context("opening the store")?;

    let token_source = if options.fresh {
        Some(
            options
                .import_tokens
                .clone()
                .unwrap_or_else(|| real.database_file()),
        )
    } else {
        options.import_tokens.clone()
    };
    if let Some(source) = token_source {
        let copied = storage
            .import_auth_tokens(source.clone())
            .await
            .with_context(|| format!("copying tokens from {}", source.display()))?;
        report.line(&format!(
            "copied {copied} auth token(s) from {} (values not shown)",
            source.display()
        ));
    }
    report.line(&format!(
        "store: {}{}",
        paths.database_file().display(),
        if options.fresh {
            " (fresh: tokens only, no captured traffic)"
        } else {
            ""
        }
    ));

    let library = Library::new(storage.clone(), paths.clone())?;
    let sync = LibrarySync::with_artwork_prefetch(storage.clone(), library.clone())?;
    let started = std::time::Instant::now();
    let snapshot = sync.sync_now().await.context("library sync failed")?;
    report.line(&format!(
        "sync: {} collections, {} distinct tracks in {:.1}s",
        snapshot.collections.len(),
        snapshot.tracks.len(),
        started.elapsed().as_secs_f32()
    ));
    report.line("albums Apple returned for /v1/me/library/albums:");
    for synced in &snapshot.collections {
        if synced.collection.kind == CollectionKind::Album {
            report.line(&format!(
                "  - {} ({})",
                synced.collection.name,
                synced.collection.artist.as_deref().unwrap_or("?")
            ));
        }
    }

    let contents = storage.library_contents(LibraryFilter::AppleMusic).await?;
    verify(&mut report, &snapshot, &contents, &library).await;
    prefetch_then_open(&mut report, &sync, &snapshot, &storage, &library).await?;

    if report.failures.is_empty() {
        report.line("RESULT: PASS");
        Ok(true)
    } else {
        report.line(&format!(
            "RESULT: FAIL ({} problem(s))",
            report.failures.len()
        ));
        Ok(false)
    }
}

#[expect(clippy::too_many_lines, reason = "one linear list of assertions")]
async fn verify(
    report: &mut Report,
    snapshot: &LibrarySnapshot,
    contents: &cider_offline_core::LibraryContents,
    library: &Library,
) {
    let apple: HashMap<&str, _> = snapshot
        .collections
        .iter()
        .map(|c| (c.collection.id.as_str(), c))
        .collect();
    let titles: HashMap<&str, &str> = contents
        .tracks
        .iter()
        .map(|t| (t.id.as_str(), t.title.as_str()))
        .collect();

    report.line("");
    report.line("Apple Music library view (as the GUI shows it):");
    let mut artwork_owner: HashMap<PathBuf, (String, String)> = HashMap::new();
    for (index, entry) in contents.collections.iter().enumerate() {
        let c = &entry.collection;
        let kind = match c.kind {
            CollectionKind::Album => "album",
            CollectionKind::Playlist => "playlist",
        };
        let Some(synced) = apple.get(c.id.as_str()) else {
            report.fail(format!("{} is not in the Apple snapshot", c.name));
            continue;
        };
        let reported = synced
            .reported_total
            .map_or_else(|| "-".to_owned(), |t| t.to_string());
        report.line(&format!(
            "{:>2}. [{kind}] {}  stored={} apple_paginated={} apple_meta_total={}",
            index + 1,
            c.name,
            entry.track_ids.len(),
            synced.track_ids.len(),
            reported
        ));
        let first: Vec<&str> = entry
            .track_ids
            .iter()
            .take(3)
            .map(|id| {
                titles
                    .get(id.as_str())
                    .copied()
                    .unwrap_or("<missing track row>")
            })
            .collect();
        report.line(&format!("      first: {first:?}"));

        if entry.track_ids != synced.track_ids {
            report.fail(format!(
                "{}: stored track list differs from Apple's (order or content)",
                c.name
            ));
        }
        if let Some(total) = synced.reported_total
            && usize::try_from(total).ok() != Some(entry.track_ids.len())
        {
            report.fail(format!(
                "{}: stored {} tracks but Apple reported {total}",
                c.name,
                entry.track_ids.len()
            ));
        }
        let apple_ids: HashSet<&str> = synced.track_ids.iter().map(String::as_str).collect();
        if let Some(stray) = entry
            .track_ids
            .iter()
            .find(|id| !apple_ids.contains(id.as_str()))
        {
            report.fail(format!("{}: track {stray} was not listed by Apple", c.name));
        }
        if let Some(missing) = entry
            .track_ids
            .iter()
            .find(|id| !titles.contains_key(id.as_str()))
        {
            report.fail(format!("{}: track {missing} has no track row", c.name));
        }

        match c.artwork_url.as_deref() {
            None => report.line("      artwork: <none>"),
            Some(url) => {
                let key = url
                    .replace('{', "%7B")
                    .replace('}', "%7D")
                    .parse::<Uri>()
                    .map(|uri| artwork::base_key(&uri))
                    .unwrap_or_default();
                let short_key: String = key.chars().take(110).collect();
                match library.ensure_artwork(url).await {
                    Some(path) => {
                        report.line(&format!(
                            "      artwork: {} (key {short_key})",
                            path.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ));
                        if let Some((owner, owner_url)) = artwork_owner.get(&path)
                            && owner_url != url
                        {
                            report.fail(format!(
                                "{} and {owner} share artwork file {} but have different artwork URLs",
                                c.name,
                                path.display()
                            ));
                        }
                        artwork_owner.insert(path, (c.name.clone(), url.to_owned()));
                    }
                    None => report.fail(format!("{}: artwork could not be fetched", c.name)),
                }
            }
        }
    }

    // The view must show exactly the collections Apple returned, each once.
    let view_ids: Vec<&str> = contents
        .collections
        .iter()
        .map(|c| c.collection.id.as_str())
        .collect();
    let unique_view_ids: HashSet<&str> = view_ids.iter().copied().collect();
    if unique_view_ids.len() != view_ids.len() {
        report.fail("the view lists a collection more than once".to_owned());
    }
    let apple_ids: HashSet<&str> = apple.keys().copied().collect();
    let extra = unique_view_ids.difference(&apple_ids).count();
    let missing = apple_ids.difference(&unique_view_ids).count();
    if extra > 0 || missing > 0 {
        report.fail(format!(
            "the view and Apple's snapshot disagree ({extra} extra, {missing} missing collection(s))"
        ));
    }
    if let Some(unnamed) = contents
        .collections
        .iter()
        .find(|c| c.collection.name.trim().is_empty())
    {
        report.fail(format!("collection {} has no name", unnamed.collection.id));
    }
    let first_album = contents
        .collections
        .iter()
        .position(|c| c.collection.kind == CollectionKind::Album);
    let last_playlist = contents
        .collections
        .iter()
        .rposition(|c| c.collection.kind == CollectionKind::Playlist);
    if let (Some(album), Some(playlist)) = (first_album, last_playlist)
        && album < playlist
    {
        report.fail("an album is ordered before a playlist".to_owned());
    }
    if contents.collections.len() != snapshot.collections.len() {
        report.fail(format!(
            "view shows {} collections, Apple returned {}",
            contents.collections.len(),
            snapshot.collections.len()
        ));
    }
    if let Some(album) = contents
        .collections
        .iter()
        .find(|c| c.collection.id.starts_with("album-group:"))
    {
        report.fail(format!(
            "synthesized album shown: {}",
            album.collection.name
        ));
    }
}

/// Runs the post-sync cover prefetch, then opens the largest playlist the way
/// the detail view does and requires that no cover needs the network.
async fn prefetch_then_open(
    report: &mut Report,
    sync: &LibrarySync,
    snapshot: &LibrarySnapshot,
    storage: &Storage,
    library: &Library,
) -> anyhow::Result<()> {
    report.line("");
    let started = std::time::Instant::now();
    if let Some(prefetch) = sync
        .prefetch_artwork(snapshot, CancellationToken::new())
        .await
    {
        report.line(&format!(
            "artwork prefetch: {} distinct covers, {} cached, {} downloaded, {} failed in {:.1}s",
            prefetch.requested,
            prefetch.cached,
            prefetch.fetched,
            prefetch.failed,
            started.elapsed().as_secs_f32()
        ));
        if prefetch.failed > 0 {
            report.fail(format!(
                "{} covers could not be prefetched",
                prefetch.failed
            ));
        }
    }
    let Some(largest) = snapshot
        .collections
        .iter()
        .max_by_key(|c| c.track_ids.len())
    else {
        return Ok(());
    };
    let query = LibraryQuery {
        collection: Some(largest.collection.id.clone()),
        ..LibraryQuery::default()
    };
    let open = crate::ui::perf::open_collection(storage, library, query).await?;
    report.line(&format!(
        "open {:?}: {} rows, {} distinct covers decoded, {} rows with cover, {} network requests, all covers in {:.0}ms",
        largest.collection.name,
        open.rows,
        open.decoded,
        open.with_cover,
        open.network,
        open.all_covers.as_secs_f64() * 1000.0
    ));
    if open.network > 0 {
        report.fail(format!(
            "opening {} needed {} cover downloads after the prefetch",
            largest.collection.name, open.network
        ));
    }
    Ok(())
}

/// Removes the temporary fresh store when the check ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Exits the process with the check's status (helper for `main`).
pub fn exit_with(result: anyhow::Result<bool>) -> anyhow::Result<()> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => bail!("library check failed"),
        Err(error) => Err(error),
    }
}
