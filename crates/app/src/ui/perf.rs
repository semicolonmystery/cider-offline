//! Hooks for the headless `--perf-check` harness: drives the same paged
//! model, slot building and cover pipeline the Library page runs, without a
//! window (`Image` creation needs no window or renderer).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cider_offline_core::{Library, LibraryQuery, Storage, StorageError};
use slint::Model as _;

use super::TrackRow;
pub use super::artwork::Bucket;
use super::artwork::{self, ArtRequest, ImageCache};
use super::library::{TRACK_PAGE, track_slot};
use super::paged::{Hooks, PagedModel};

/// Measurements of opening one collection detail.
#[derive(Debug, Default)]
pub struct OpenReport {
    /// Member rows (the model's row count).
    pub rows: usize,
    /// Page queries for every member.
    pub query: Duration,
    /// Building every row slot.
    pub build: Duration,
    /// From the first query until the first 20 rows show their covers.
    pub first_screen: Duration,
    /// From the first query until every row shows its cover.
    pub all_covers: Duration,
    /// Cover requests the rows made (one per row with artwork).
    pub requests: usize,
    /// Distinct images decoded (= `Image`s created) for all rows.
    pub decoded: usize,
    /// Covers downloaded from the CDN while opening.
    pub network: usize,
    /// Rows that ended up showing a cover.
    pub with_cover: usize,
    /// `row_changed` notifications sent (in-place updates, no rebuilds).
    pub notified: usize,
}

/// Opens `query` (a collection's members) like the detail view does, then
/// scrolls through every row.
pub async fn open_collection(
    storage: &Storage,
    library: &Library,
    query: LibraryQuery,
) -> Result<OpenReport, StorageError> {
    let mut report = OpenReport::default();
    let started = Instant::now();
    let mut pages = vec![storage.tracks_page(query.clone(), 0, TRACK_PAGE).await?];
    let total = pages[0].total;
    let mut offset = TRACK_PAGE;
    while offset < total {
        pages.push(
            storage
                .tracks_page(query.clone(), offset, TRACK_PAGE)
                .await?,
        );
        offset += TRACK_PAGE;
    }
    report.query = started.elapsed();

    let built = Instant::now();
    let mut images = ImageCache::default();
    let downloads = HashMap::new();
    let mut slot_pages: Vec<_> = pages
        .into_iter()
        .map(|page| {
            page.items
                .into_iter()
                .map(|item| track_slot(item, &mut images, &downloads))
                .collect::<Vec<_>>()
        })
        .collect();
    report.build = built.elapsed();

    let model: PagedModel<TrackRow> = PagedModel::new(TRACK_PAGE, 400);
    let wanted: Rc<RefCell<Vec<ArtRequest>>> = Rc::default();
    let sink = Rc::clone(&wanted);
    model.set_hooks(Hooks {
        fetch: Box::new(|_, _| {}),
        want_art: Box::new(move |requests| sink.borrow_mut().extend(requests)),
    });
    let generation = model.begin();
    let first = if slot_pages.is_empty() {
        Vec::new()
    } else {
        slot_pages.remove(0)
    };
    model.reset(generation, total, first);
    for (index, slots) in slot_pages.into_iter().enumerate() {
        model.apply_page(generation, index + 1, total, slots);
    }
    report.rows = model.row_count();

    // First screen, then the rest, exactly as scrolling would request them.
    let screen = report.rows.min(20);
    for window in [0..screen, screen..report.rows] {
        for row in window {
            model.row_data(row);
        }
        let requests = std::mem::take(&mut *wanted.borrow_mut());
        report.requests += requests.len();
        let (hits, misses) = images.triage(requests);
        model.apply_images(&hits);
        let decoded = artwork::load(library.clone(), misses).await;
        report.network += decoded.iter().filter(|d| d.fetched).count();
        let created = images.accept(decoded);
        model.apply_images(&created);
        if report.first_screen.is_zero() {
            report.first_screen = started.elapsed();
        }
    }
    report.all_covers = started.elapsed();
    report.decoded = images.stats().0;
    report.notified = model.stats().2;
    report.with_cover = (0..report.rows)
        .filter_map(|row| model.row_data(row))
        .filter(|row| row.has_artwork)
        .count();
    Ok(report)
}

/// Builds the first page of rows for `query` (the songs list) and returns
/// (rows, build time).
pub fn build_first_page(items: Vec<cider_offline_core::LibraryTrack>) -> (usize, Duration) {
    let started = Instant::now();
    let mut images = ImageCache::default();
    let downloads = HashMap::new();
    let rows = items
        .into_iter()
        .map(|item| track_slot(item, &mut images, &downloads))
        .count();
    (rows, started.elapsed())
}

/// Resolves one cover for `bucket`; returns (requested URL, source pixels,
/// decoded width, decoded height).
pub async fn resolve_cover(
    library: &Library,
    url: &str,
    bucket: Bucket,
) -> Option<(String, u64, u32, u32)> {
    let requested = cider_offline_core::artwork::sized_url(url, bucket.source_edge());
    let mut decoded = artwork::load(
        library.clone(),
        vec![ArtRequest::new(url.to_owned(), bucket)],
    )
    .await;
    let item = decoded.pop()?;
    let pixels = item.pixels?;
    Some((
        requested,
        item.source_pixels,
        pixels.width(),
        pixels.height(),
    ))
}
