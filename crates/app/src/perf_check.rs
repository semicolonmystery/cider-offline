//! Headless performance check (`--perf-check`): times the paged library
//! queries, the cover prefetch, opening a large playlist (queries, row
//! building, cover decoding) and verifies cover resolutions, against the
//! app's real store. No window and no proxy; the network is used only by the
//! cover prefetch for covers missing from the cache.
//!
//! ```text
//! cider-offline --perf-check [--collection NAME]
//! ```
//!
//! Without `--collection`, the largest collection in the Apple Music library
//! is opened.

use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use cider_offline_core::artwork::{LARGE_EDGE, SMALL_EDGE};
use cider_offline_core::{
    ArtworkRequest, CancellationToken, Library, LibraryFilter, LibraryQuery, Storage, TrackFilter,
};

use crate::ui::perf;

/// The flag that selects this mode.
pub const FLAG: &str = "--perf-check";

/// Options parsed from the command line.
#[derive(Debug, Default)]
pub struct Options {
    /// The collection to open; `None` picks the largest one.
    collection: Option<String>,
}

/// Returns the options when `--perf-check` is present.
pub fn parse(args: &[String]) -> Option<Options> {
    if !args.iter().any(|a| a == FLAG) {
        return None;
    }
    let collection = args
        .iter()
        .position(|a| a == "--collection")
        .and_then(|i| args.get(i + 1))
        .cloned();
    Some(Options { collection })
}

/// Runs the check.
pub fn run(options: &Options) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating the tokio runtime")?;
    rt.block_on(check(options.collection.as_deref()))
}

fn line(text: &str) {
    // Best effort: a closed stdout must not abort the check.
    let _ = writeln!(std::io::stdout().lock(), "{text}");
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

const FILTERS: [LibraryFilter; 4] = [
    LibraryFilter::AppleMusic,
    LibraryFilter::Downloaded,
    LibraryFilter::All,
    LibraryFilter::Custom,
];

/// Best of three runs of `f`.
async fn best_of<T, F, Fut>(mut f: F) -> anyhow::Result<(Duration, T)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut best = Duration::MAX;
    let mut last = None;
    for _ in 0..3 {
        let started = Instant::now();
        let value = f().await?;
        best = best.min(started.elapsed());
        last = Some(value);
    }
    Ok((best, last.context("no run")?))
}

async fn check(collection: Option<&str>) -> anyhow::Result<()> {
    let paths = crate::startup::discover_paths()?;
    let storage = Storage::open_async(paths.database_file())
        .await
        .context("opening the store")?;
    let library = Library::new(storage.clone(), paths.clone())?;
    line(&format!("store: {}", paths.database_file().display()));

    queries(&storage).await?;
    prefetch(&storage, &library).await?;
    let target = match collection {
        Some(name) => find_collection(&storage, name).await?,
        None => largest_collection(&storage).await?,
    };
    open(&storage, &library, &target).await?;
    resolutions(&storage, &library, &target).await?;
    Ok(())
}

async fn queries(storage: &Storage) -> anyhow::Result<()> {
    line("");
    line("queries per filter (best of 3; first page = 200 songs / 120 collections):");
    for filter in FILTERS {
        let (full, contents) =
            best_of(|| async { Ok(storage.library_contents(filter).await?) }).await?;
        let query = LibraryQuery {
            filter,
            ..LibraryQuery::default()
        };
        // A write invalidates the grouped view; time a rebuild once.
        let started = Instant::now();
        let cold = storage.collections_page(query.clone(), 0, 120).await?;
        let cold_time = started.elapsed();
        let (collections, page) =
            best_of(|| async { Ok(storage.collections_page(query.clone(), 0, 120).await?) })
                .await?;
        let (tracks, songs) =
            best_of(|| async { Ok(storage.tracks_page(query.clone(), 0, 200).await?) }).await?;
        let (search, found) = best_of(|| async {
            let q = LibraryQuery {
                search: "the".to_owned(),
                ..query.clone()
            };
            Ok(storage.tracks_page(q, 0, 200).await?)
        })
        .await?;
        let (missing, without) = best_of(|| async {
            let q = LibraryQuery {
                tracks: TrackFilter::MissingAudio,
                ..query.clone()
            };
            Ok(storage.tracks_page(q, 0, 200).await?)
        })
        .await?;
        let (_, (rows, build)) = best_of(|| async {
            let page = storage.tracks_page(query.clone(), 0, 200).await?;
            Ok(perf::build_first_page(page.items))
        })
        .await?;
        line(&format!(
            "  {filter:?}: library_contents {} ({} coll, {} tracks) | collections page {} (cold {}, total {}) \
             | songs page {} (total {}) | search 'the' {} ({} hits) | missing audio {} ({}) | build {rows} rows {}",
            ms(full),
            contents.collections.len(),
            contents.tracks.len(),
            ms(collections),
            ms(cold_time),
            page.total.max(cold.total),
            ms(tracks),
            songs.total,
            ms(search),
            found.total,
            ms(missing),
            without.total,
            ms(build),
        ));
    }
    Ok(())
}

async fn prefetch(storage: &Storage, library: &Library) -> anyhow::Result<()> {
    line("");
    let contents = storage.library_contents(LibraryFilter::AppleMusic).await?;
    let mut requests: Vec<ArtworkRequest> = contents
        .collections
        .iter()
        .filter_map(|c| c.collection.artwork_url.clone())
        .map(|url| ArtworkRequest {
            url,
            edge: LARGE_EDGE,
        })
        .collect();
    requests.extend(
        contents
            .tracks
            .iter()
            .filter_map(|t| t.artwork_url.clone())
            .map(|url| ArtworkRequest {
                url,
                edge: SMALL_EDGE,
            }),
    );
    let raw = requests.len();
    let started = Instant::now();
    let report = library
        .prefetch_artwork(requests, 6, CancellationToken::new())
        .await;
    line(&format!(
        "prefetch (Apple Music library, concurrency 6): {raw} cover refs -> {} distinct, {} cached, {} downloaded, {} failed in {}",
        report.requested,
        report.cached,
        report.fetched,
        report.failed,
        ms(started.elapsed())
    ));
    Ok(())
}

/// A collection picked for the check: its id and display name.
struct Target {
    id: String,
    name: String,
}

/// The Apple Music collection with the most tracks.
async fn largest_collection(storage: &Storage) -> anyhow::Result<Target> {
    let contents = storage.library_contents(LibraryFilter::AppleMusic).await?;
    contents
        .collections
        .into_iter()
        .max_by_key(|c| c.track_ids.len())
        .map(|c| Target {
            id: c.collection.id,
            name: c.collection.name,
        })
        .context("the Apple Music library has no collections; sync it first or pass --collection")
}

async fn find_collection(storage: &Storage, name: &str) -> anyhow::Result<Target> {
    let page = storage
        .collections_page(
            LibraryQuery {
                search: name.to_owned(),
                ..LibraryQuery::default()
            },
            0,
            50,
        )
        .await?;
    page.items
        .into_iter()
        .find(|c| c.collection.name.trim() == name)
        .map(|c| Target {
            id: c.collection.id,
            name: c.collection.name,
        })
        .with_context(|| format!("collection {name:?} not in the Apple Music library"))
}

async fn open(storage: &Storage, library: &Library, target: &Target) -> anyhow::Result<()> {
    line("");
    let name = &target.name;
    let query = LibraryQuery {
        collection: Some(target.id.clone()),
        ..LibraryQuery::default()
    };
    let report = perf::open_collection(storage, library, query).await?;
    line(&format!(
        "open {name:?} (paged model, warm disk cache): {} rows",
        report.rows
    ));
    line(&format!(
        "  queries {} | row build {} | first 20 covers on screen after {} | all covers after {}",
        ms(report.query),
        ms(report.build),
        ms(report.first_screen),
        ms(report.all_covers)
    ));
    line(&format!(
        "  cover requests {} -> distinct decoded images {} | rows with cover {} | row notifications {} | network requests on open {}",
        report.requests, report.decoded, report.with_cover, report.notified, report.network
    ));
    Ok(())
}

async fn resolutions(storage: &Storage, library: &Library, target: &Target) -> anyhow::Result<()> {
    line("");
    let collections = storage
        .collections_page(LibraryQuery::default(), 0, 120)
        .await?;
    if let Some(url) = collections
        .items
        .iter()
        .find_map(|c| c.collection.artwork_url.clone())
        && let Some((requested, source, w, h)) =
            perf::resolve_cover(library, &url, perf::Bucket::Grid).await
    {
        line(&format!(
            "grid cover: requested {requested}\n  source {source} px ({}x{0}), decoded {w}x{h}",
            source.isqrt()
        ));
    }
    let tracks = storage
        .tracks_page(
            LibraryQuery {
                collection: Some(target.id.clone()),
                ..LibraryQuery::default()
            },
            0,
            1,
        )
        .await?;
    if let Some(url) = tracks
        .items
        .first()
        .and_then(|t| t.track.artwork_url.clone())
        && let Some((requested, source, w, h)) =
            perf::resolve_cover(library, &url, perf::Bucket::Row).await
    {
        line(&format!(
            "row thumb: requested {requested}\n  source {source} px ({}x{0}), decoded {w}x{h}",
            source.isqrt()
        ));
    }
    Ok(())
}
