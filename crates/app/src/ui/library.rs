//! Library page: cached collections, searchable track list and row actions.
//!
//! Both lists are [`PagedModel`]s over the paged core queries
//! ([`cider_offline_core::Storage::tracks_page`] / `collections_page`): only the
//! pages a view actually shows are queried and built, search and filters run
//! in SQL, and row updates (download progress, busy flags, covers) notify only
//! the rows that changed. Covers are decoded off the UI thread and shared
//! through one [`ImageCache`] (see [`super::artwork`]).
//!
//! Entry points for the views (infinite scroll needs nothing else):
//! `LibraryState.tracks` / `LibraryState.collections` report the full match
//! count as their length and load pages on demand from `row_data`; rows not
//! loaded yet have `placeholder: true`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use cider_offline_core::storage::{
    AudioVariant, CollectionKind, LyricsSource, Playlist, Track, TrackMapping,
};
use cider_offline_core::{
    CollectionDownload, CollectionDownloadState, DownloadCollection, DownloadFormat,
    DownloadRecord, DownloadSourceKind, LibraryCollection, LibraryFilter, LibraryPage,
    LibraryQuery, LibraryTrack, LyricsProgress, LyricsProviderId, LyricsReport, StorageError,
    SyncStatus, TrackFilter, record_from_track, track_needs_download,
};
use slint::{ComponentHandle as _, Image, ModelRc, TimerMode};

use super::artwork::{self, ArtKey, ArtRequest, Bucket, ImageCache};
use super::downloads::QueueAction;
use super::paged::{Hooks, PagedModel, Slot};
use super::{AppContext, CollectionRow, LibraryState, TrackRow, dialogs, format};

/// Tracks per page (one query, one batch of rows).
pub const TRACK_PAGE: usize = 200;
/// Collections per page.
pub const COLLECTION_PAGE: usize = 120;
/// Track rows that may hold a decoded thumbnail at once (about 15 MB at 96 px).
const TRACK_IMAGE_ROWS: usize = 400;
/// Collection cards that may hold a decoded cover at once (about 70 MB at 384 px).
const COLLECTION_IMAGE_ROWS: usize = 120;
/// Search runs this long after the last keystroke.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Covers per background decode batch, so they appear progressively.
const ART_CHUNK: usize = 12;
/// Grid cards wider than this (logical px) decode covers at the large size.
const LARGE_CARD_WIDTH: f32 = 192.0;
/// Download events of one burst (a finished song, a removal) are gathered
/// this long before the collections' download state is read again.
const DOWNLOAD_STATE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Download summaries of collections, by collection id.
type DownloadSummaries = HashMap<String, CollectionDownload>;

/// Track-list filter selected in the combo box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Filter {
    #[default]
    All,
    MissingAudio,
    MissingLyrics,
}

impl Filter {
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::MissingAudio,
            2 => Self::MissingLyrics,
            _ => Self::All,
        }
    }

    fn core(self) -> TrackFilter {
        match self {
            Self::All => TrackFilter::All,
            Self::MissingAudio => TrackFilter::MissingAudio,
            Self::MissingLyrics => TrackFilter::MissingLyrics,
        }
    }
}

/// Library source selected in the segmented control. Each variant maps 1:1 to a
/// core [`LibraryFilter`], in the same order as the Segmented control's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Source {
    /// The user's Apple Music library (their saved collections / songs).
    #[default]
    AppleMusic,
    /// Only collections / songs available locally in the proxy.
    Downloaded,
    /// Everything the proxy has observed, regardless of provenance.
    All,
    /// User-created custom collections / songs only.
    Custom,
}

impl Source {
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Downloaded,
            2 => Self::All,
            3 => Self::Custom,
            _ => Self::AppleMusic,
        }
    }

    /// The core library view this source selects.
    fn filter(self) -> LibraryFilter {
        match self {
            Self::AppleMusic => LibraryFilter::AppleMusic,
            Self::Downloaded => LibraryFilter::Downloaded,
            Self::All => LibraryFilter::All,
            Self::Custom => LibraryFilter::Custom,
        }
    }
}

/// The collection open in the detail view.
struct OpenDetail {
    id: String,
    kind: String,
    name: String,
    /// The header cover (detail bucket).
    hero: Option<ArtRequest>,
    /// How much of it is downloaded (read when opened and after downloads).
    download: CollectionDownload,
    /// User-created collection.
    is_custom: bool,
}

/// What the detail header says about a collection's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MembersInfo {
    /// Number of member songs.
    pub songs: usize,
    /// Sum of the known song durations.
    pub total_ms: u64,
    /// How much of the collection is downloaded.
    pub download: CollectionDownload,
}

/// Library state owned by the UI thread.
pub struct LibraryData {
    /// Songs view / search results / open collection's members (paged).
    tracks: Rc<PagedModel<TrackRow>>,
    /// Collections grid (paged).
    collections: Rc<PagedModel<CollectionRow>>,
    /// Decoded covers shared by every row, card and the detail header.
    images: RefCell<ImageCache>,
    /// Covers requested by rows, flushed once per event-loop turn.
    art_queue: RefCell<Vec<ArtRequest>>,
    art_flush_scheduled: Cell<bool>,
    /// Search text as typed.
    search: RefCell<String>,
    /// Search text of the current query (applied after the debounce).
    applied_search: RefCell<String>,
    search_timer: slint::Timer,
    filter: Cell<Filter>,
    /// Apple Music library vs. only locally-downloaded items.
    source: Cell<Source>,
    /// Decode size of grid covers (follows the card width).
    grid_bucket: Cell<Bucket>,
    /// Status messages shown so far (drives the toast restart).
    message_seq: Cell<i32>,
    open: RefCell<Option<OpenDetail>>,
    /// Every collection of the source as `(id, name, subtitle, is_custom)`,
    /// for the Add dialog's placement dropdown.
    directory: RefCell<Vec<(String, String, String, bool)>>,
    /// Collections with a download pin (downloaded as a whole).
    pinned: RefCell<HashSet<String>>,
    /// Collections set to "Keep downloaded".
    kept: RefCell<HashSet<String>>,
    /// Re-reads the collections' download state once a burst of download
    /// events is over (see [`refresh_download_state`]).
    download_state_timer: slint::Timer,
}

impl Default for LibraryData {
    fn default() -> Self {
        Self {
            tracks: Rc::new(PagedModel::new(TRACK_PAGE, TRACK_IMAGE_ROWS)),
            collections: Rc::new(PagedModel::new(COLLECTION_PAGE, COLLECTION_IMAGE_ROWS)),
            images: RefCell::new(ImageCache::default()),
            art_queue: RefCell::new(Vec::new()),
            art_flush_scheduled: Cell::new(false),
            search: RefCell::new(String::new()),
            applied_search: RefCell::new(String::new()),
            search_timer: slint::Timer::default(),
            filter: Cell::new(Filter::default()),
            source: Cell::new(Source::default()),
            grid_bucket: Cell::new(Bucket::Grid),
            message_seq: Cell::new(0),
            open: RefCell::new(None),
            directory: RefCell::new(Vec::new()),
            pinned: RefCell::new(HashSet::new()),
            kept: RefCell::new(HashSet::new()),
            download_state_timer: slint::Timer::default(),
        }
    }
}

impl LibraryData {
    /// Snapshot of the source's collections as `(id, name, subtitle, is_custom)`,
    /// used to build the Add dialog's placement dropdown without a DB round-trip.
    pub(super) fn collection_rows_snapshot(&self) -> Vec<(String, String, String, bool)> {
        self.directory.borrow().clone()
    }
}

/// Wires the Library callbacks and the paged models.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<LibraryState>();
    state.set_tracks(ModelRc::from(Rc::clone(&ctx.library.tracks)));
    state.set_collections(ModelRc::from(Rc::clone(&ctx.library.collections)));
    install_hooks(ctx);

    let c = Rc::clone(ctx);
    state.on_refresh(move || refresh(&c));
    let c = Rc::clone(ctx);
    state.on_search_changed(move |text| search_changed(&c, text.as_str()));
    let c = Rc::clone(ctx);
    state.on_filter_changed(move |index| {
        c.library.filter.set(Filter::from_index(index));
        requery_tracks(&c);
    });
    let c = Rc::clone(ctx);
    state.on_set_source(move |index| {
        c.library.source.set(Source::from_index(index));
        // The source maps to a core LibraryFilter, so re-query.
        reload(&c);
    });
    // Both views read the same models; switching needs no query.
    state.on_set_view(|_view| {});
    let c = Rc::clone(ctx);
    state.on_open_collection(move |id| open_collection(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_close_collection(move || close_collection(&c));
    let c = Rc::clone(ctx);
    state.on_import_audio(move |id| import_audio(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_map_audio(move |id| map_audio(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_clear_mapping(move |id| clear_mapping(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_refetch_lyrics(move |id| refetch_lyrics(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_choose_lyrics_file(move |id| choose_lyrics_file(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_remove_track(move |id| remove_track(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_remove_download(move |id| remove_download(&c, id.into()));
    let c = Rc::clone(ctx);
    state
        .on_remove_collection_download(move |id| confirm_remove_collection_download(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_collection_download_action(move |id, action| {
        collection_download_action(&c, id.as_str(), action.as_str());
    });
    let c = Rc::clone(ctx);
    state.on_set_keep_downloaded(move |id, keep| set_keep_downloaded(&c, id.as_str(), keep));
    let c = Rc::clone(ctx);
    state.on_fetch_missing_lyrics(move || fetch_missing_lyrics(&c));
    let c = Rc::clone(ctx);
    state.on_fetch_collection_lyrics(move |id| fetch_collection_lyrics(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_remove_collection_lyrics(move |id| confirm_remove_collection_lyrics(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_remove_lyrics(move |id| remove_song_lyrics(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_download_track(move |id| download_track(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_download_collection(move |id| download_collection(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_redownload_track(move |id| redownload_track(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_redownload_collection(move |id| redownload_collection(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_toggle_collection_download(move |id| {
        super::downloads::toggle_collection(&c, id.as_str());
    });
    let c = Rc::clone(ctx);
    state.on_download_missing_audio(move || download_missing_audio(&c));
    let c = Rc::clone(ctx);
    state.on_add_item(move || super::library_add::open_add(&c));
    let c = Rc::clone(ctx);
    state.on_edit_item(move |id| super::library_add::open_edit(&c, id.into()));
    let c = Rc::clone(ctx);
    state.on_reorder_track(move |id, position| reorder_track(&c, id.as_str(), position));
    let c = Rc::clone(ctx);
    state.on_card_width_changed(move |width| card_width_changed(&c, width));
}

/// Picks the grid cover decode size for the current card width; when it
/// changes, the cards are rebuilt so they request the new size.
fn card_width_changed(ctx: &Rc<AppContext>, width: f32) {
    if width <= 0.0 {
        return;
    }
    let bucket = grid_bucket_for(width);
    if ctx.library.grid_bucket.replace(bucket) != bucket {
        requery_collections(ctx);
    }
}

/// The grid decode bucket for a card `width` (logical px): 384 px covers up
/// to 192 px cards, 512 px (shared with the detail header) above that.
pub(super) fn grid_bucket_for(width: f32) -> Bucket {
    if width > LARGE_CARD_WIDTH {
        Bucket::Hero
    } else {
        Bucket::Grid
    }
}

/// Connects the paged models to page queries and the cover loader. The
/// models hold only a weak context handle (no `Rc` cycle).
fn install_hooks(ctx: &Rc<AppContext>) {
    let weak = Rc::downgrade(ctx);
    let art = {
        let weak = weak.clone();
        move |requests: Vec<ArtRequest>| {
            if let Some(ctx) = weak.upgrade() {
                queue_art(&ctx, requests);
            }
        }
    };
    let fetch_weak = weak.clone();
    ctx.library.tracks.set_hooks(Hooks {
        fetch: Box::new(move |generation, page| {
            if let Some(ctx) = fetch_weak.upgrade() {
                fetch_tracks_page(&ctx, generation, page);
            }
        }),
        want_art: Box::new(art.clone()),
    });
    ctx.library.collections.set_hooks(Hooks {
        fetch: Box::new(move |generation, page| {
            if let Some(ctx) = weak.upgrade() {
                fetch_collections_page(&ctx, generation, page);
            }
        }),
        want_art: Box::new(art),
    });
}

// ----- Queries ---------------------------------------------------------------

/// The query behind the track list: the source's songs with the applied
/// search and the audio / lyrics filter, or, while a collection is open, all
/// of its members (the library's search and filters don't apply inside it).
fn track_query(ctx: &AppContext) -> LibraryQuery {
    let data = &ctx.library;
    let filter = data.source.get().filter();
    match data.open.borrow().as_ref() {
        Some(open) => detail_query(filter, &open.id),
        None => LibraryQuery {
            filter,
            search: data.applied_search.borrow().clone(),
            tracks: data.filter.get().core(),
            collection: None,
        },
    }
}

/// Every member of collection `id`, unfiltered.
fn detail_query(filter: LibraryFilter, id: &str) -> LibraryQuery {
    LibraryQuery {
        filter,
        collection: Some(id.to_owned()),
        ..LibraryQuery::default()
    }
}

/// The query behind the collections grid.
fn collection_query(ctx: &AppContext) -> LibraryQuery {
    let data = &ctx.library;
    LibraryQuery {
        filter: data.source.get().filter(),
        search: data.applied_search.borrow().clone(),
        ..LibraryQuery::default()
    }
}

/// Builds the model slot of a track row: its cover request (row bucket), the
/// cached image if any, and the live download state.
pub(super) fn track_slot(
    item: LibraryTrack,
    images: &mut ImageCache,
    downloads: &HashMap<String, (String, f32)>,
) -> Slot<TrackRow> {
    let art = item
        .track
        .artwork_url
        .clone()
        .map(|url| ArtRequest::new(url, Bucket::Row));
    let image = art.as_ref().and_then(|art| images.get(&art.key));
    let mut row = track_row(item.track, image, item.has_mapping);
    set_track_dl(&mut row, downloads);
    Slot { row, art }
}

/// Builds the model slot of a collection card (decoded at `bucket`).
pub(super) fn collection_slot(
    entry: LibraryCollection,
    bucket: Bucket,
    images: &mut ImageCache,
    downloads: &HashMap<String, (String, f32)>,
    summaries: &DownloadSummaries,
) -> Slot<CollectionRow> {
    let art = entry
        .collection
        .artwork_url
        .clone()
        .map(|url| ArtRequest::new(url, bucket));
    let image = art.as_ref().and_then(|art| images.get(&art.key));
    let mut row = collection_row(entry.collection, image);
    set_collection_dl(&mut row, downloads);
    let summary = summaries.get(row.id.as_str()).copied().unwrap_or_default();
    apply_collection_download(&mut row, summary);
    Slot { row, art }
}

fn track_slots(ctx: &AppContext, items: Vec<LibraryTrack>) -> Vec<Slot<TrackRow>> {
    let downloads = super::downloads::track_progress(ctx);
    let format = ctx.config().download_format;
    let mut images = ctx.library.images.borrow_mut();
    items
        .into_iter()
        .map(|item| {
            let missing = item.needs_download(format);
            let mut slot = track_slot(item, &mut images, &downloads);
            slot.row.missing = missing;
            slot
        })
        .collect()
}

fn collection_slots(
    ctx: &AppContext,
    items: Vec<LibraryCollection>,
    summaries: &DownloadSummaries,
) -> Vec<Slot<CollectionRow>> {
    let downloads = super::downloads::collection_progress(ctx);
    let bucket = ctx.library.grid_bucket.get();
    let pinned = ctx.library.pinned.borrow();
    let kept = ctx.library.kept.borrow();
    let mut images = ctx.library.images.borrow_mut();
    items
        .into_iter()
        .map(|item| {
            let mut slot = collection_slot(item, bucket, &mut images, &downloads, summaries);
            slot.row.pinned = pinned.contains(slot.row.id.as_str());
            slot.row.keep = kept.contains(slot.row.id.as_str());
            slot
        })
        .collect()
}

fn fetch_tracks_page(ctx: &Rc<AppContext>, generation: u64, page: usize) {
    let query = track_query(ctx);
    let size = ctx.library.tracks.page_size();
    ctx.with_backend(
        move |backend| async move { backend.storage.tracks_page(query, page * size, size).await },
        move |ctx, result| match result {
            Some(Ok(page_rows)) => {
                let LibraryPage { items, total, .. } = page_rows;
                let slots = track_slots(ctx, items);
                ctx.library
                    .tracks
                    .apply_page(generation, page, total, slots);
            }
            Some(Err(error)) => show_error(ctx, &format!("Could not load songs: {error}")),
            None => {}
        },
    );
}

fn fetch_collections_page(ctx: &Rc<AppContext>, generation: u64, page: usize) {
    let query = collection_query(ctx);
    let size = ctx.library.collections.page_size();
    let format = ctx.config().download_format;
    ctx.with_backend(
        move |backend| async move {
            let filter = query.filter;
            let page = backend
                .storage
                .collections_page(query, page * size, size)
                .await?;
            let summaries = page_summaries(&backend.storage, filter, &page, format).await?;
            Ok::<_, StorageError>((page, summaries))
        },
        move |ctx, result| match result {
            Some(Ok((page_rows, summaries))) => {
                let LibraryPage { items, total, .. } = page_rows;
                let slots = collection_slots(ctx, items, &summaries);
                ctx.library
                    .collections
                    .apply_page(generation, page, total, slots);
            }
            Some(Err(error)) => {
                show_error(ctx, &format!("Could not load the collections: {error}"));
            }
            None => {}
        },
    );
}

/// Re-runs the track query from the first page (filter, search or detail
/// changed). The old rows stay until the new first page arrives.
fn requery_tracks(ctx: &Rc<AppContext>) {
    let generation = ctx.library.tracks.begin();
    let query = track_query(ctx);
    ctx.with_backend(
        move |backend| async move { backend.storage.tracks_page(query, 0, TRACK_PAGE).await },
        move |ctx, result| match result {
            Some(Ok(page)) => {
                let slots = track_slots(ctx, page.items);
                ctx.library.tracks.reset(generation, page.total, slots);
            }
            Some(Err(error)) => show_error(ctx, &format!("Could not load songs: {error}")),
            None => {}
        },
    );
}

/// Re-runs the collections query from the first page.
fn requery_collections(ctx: &Rc<AppContext>) {
    let generation = ctx.library.collections.begin();
    let query = collection_query(ctx);
    let format = ctx.config().download_format;
    ctx.with_backend(
        move |backend| async move {
            let filter = query.filter;
            let page = backend
                .storage
                .collections_page(query, 0, COLLECTION_PAGE)
                .await?;
            let summaries = page_summaries(&backend.storage, filter, &page, format).await?;
            Ok::<_, StorageError>((page, summaries))
        },
        move |ctx, result| match result {
            Some(Ok((page, summaries))) => {
                let slots = collection_slots(ctx, page.items, &summaries);
                ctx.library.collections.reset(generation, page.total, slots);
            }
            Some(Err(error)) => {
                show_error(ctx, &format!("Could not load the collections: {error}"));
            }
            None => {}
        },
    );
}

/// The download summaries of the collections of `page`.
async fn page_summaries(
    storage: &cider_offline_core::Storage,
    filter: LibraryFilter,
    page: &LibraryPage<LibraryCollection>,
    format: DownloadFormat,
) -> Result<DownloadSummaries, StorageError> {
    let ids = page
        .items
        .iter()
        .map(|entry| entry.collection.id.clone())
        .collect();
    storage.collection_downloads(filter, ids, format).await
}

/// Stores the typed search text and re-queries 150 ms after the last
/// keystroke (earlier in-flight queries are dropped by generation).
fn search_changed(ctx: &Rc<AppContext>, text: &str) {
    text.clone_into(&mut ctx.library.search.borrow_mut());
    let weak: Weak<AppContext> = Rc::downgrade(ctx);
    ctx.library
        .search_timer
        .start(TimerMode::SingleShot, SEARCH_DEBOUNCE, move || {
            if let Some(ctx) = weak.upgrade() {
                apply_search(&ctx);
            }
        });
}

fn apply_search(ctx: &Rc<AppContext>) {
    let typed = ctx.library.search.borrow().clone();
    if *ctx.library.applied_search.borrow() == typed {
        return;
    }
    *ctx.library.applied_search.borrow_mut() = typed;
    requery_tracks(ctx);
    requery_collections(ctx);
}

/// Refresh button: asks the core to re-sync the Apple Music library from the
/// API (the page reloads again when that finishes, see [`on_library_synced`])
/// and reloads what is stored now.
fn refresh(ctx: &Rc<AppContext>) {
    ctx.with_backend(
        |backend| async move { backend.library_sync.request_sync() },
        |_, _| {},
    );
    reload(ctx);
}

/// A library sync run finished: reload on success, report a failure.
pub(super) fn on_library_synced(ctx: &Rc<AppContext>, status: &SyncStatus) {
    match &status.last_error {
        None => reload(ctx),
        Some(error) => show_error(ctx, &format!("Library sync failed: {error}")),
    }
}

/// Everything a full reload shows first.
struct Reloaded {
    tracks: LibraryPage<LibraryTrack>,
    collections: LibraryPage<LibraryCollection>,
    /// How much of each collection of the first page is downloaded.
    summaries: DownloadSummaries,
    /// Songs / collections of the source without search or filter.
    source_tracks: usize,
    directory: Vec<LibraryCollection>,
    members: Option<MembersInfo>,
    /// Collections with a download pin.
    pinned: Vec<String>,
    /// Collections set to "Keep downloaded".
    kept: Vec<String>,
}

/// Reloads the library from the store, scoped to the selected source filter:
/// the first page of each list, the source totals and the Add-dialog
/// directory, in one round trip.
pub fn reload(ctx: &Rc<AppContext>) {
    set_loading(ctx, true);
    ctx.library.images.borrow_mut().retry_failed();
    let tracks_generation = ctx.library.tracks.begin();
    let collections_generation = ctx.library.collections.begin();
    let tracks_query = track_query(ctx);
    let collections_query = collection_query(ctx);
    let filter = collections_query.filter;
    let format = ctx.config().download_format;
    ctx.with_backend(
        move |backend| async move {
            let storage = backend.storage;
            let members = match &tracks_query.collection {
                Some(id) => Some(members_info(&storage, filter, id, format).await?),
                None => None,
            };
            let tracks = storage.tracks_page(tracks_query, 0, TRACK_PAGE).await?;
            let collections = storage
                .collections_page(collections_query, 0, COLLECTION_PAGE)
                .await?;
            let summaries = page_summaries(&storage, filter, &collections, format).await?;
            let source = LibraryQuery {
                filter,
                ..LibraryQuery::default()
            };
            let source_tracks = storage.tracks_page(source.clone(), 0, 0).await?.total;
            let directory = storage.collections_page(source, 0, usize::MAX).await?.items;
            let pinned = storage.pinned_collections().await?;
            let kept = kept_collections(&storage).await?;
            Ok::<_, StorageError>(Reloaded {
                tracks,
                collections,
                summaries,
                source_tracks,
                directory,
                members,
                pinned,
                kept,
            })
        },
        move |ctx, result| {
            set_loading(ctx, false);
            match result {
                Some(Ok(reloaded)) => {
                    show_reloaded(ctx, reloaded, tracks_generation, collections_generation);
                }
                Some(Err(error)) => {
                    show_error(ctx, &format!("Could not load the library: {error}"));
                }
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

fn show_reloaded(
    ctx: &Rc<AppContext>,
    reloaded: Reloaded,
    tracks_generation: u64,
    collections_generation: u64,
) {
    let Reloaded {
        tracks,
        collections,
        summaries,
        source_tracks,
        directory,
        members,
        pinned,
        kept,
    } = reloaded;
    *ctx.library.pinned.borrow_mut() = pinned.into_iter().collect();
    *ctx.library.kept.borrow_mut() = kept.into_iter().collect();
    let source_collections = directory.len();
    *ctx.library.directory.borrow_mut() = directory
        .into_iter()
        .map(|entry| {
            let row = collection_row(entry.collection, None);
            (
                row.id.to_string(),
                row.name.to_string(),
                row.subtitle.to_string(),
                row.is_custom,
            )
        })
        .collect();
    if let Some(info) = members {
        apply_members_info(ctx, info);
    }
    let track_slots = track_slots(ctx, tracks.items);
    ctx.library
        .tracks
        .reset(tracks_generation, tracks.total, track_slots);
    let collection_slots = collection_slots(ctx, collections.items, &summaries);
    ctx.library
        .collections
        .reset(collections_generation, collections.total, collection_slots);
    ctx.with_ui(|ui| {
        let state = ui.global::<LibraryState>();
        state.set_total_tracks(i32::try_from(source_tracks).unwrap_or(i32::MAX));
        state.set_total_collections(i32::try_from(source_collections).unwrap_or(i32::MAX));
        state.set_count_line(format::library_counts(source_collections, source_tracks).into());
    });
    update_detail_download(ctx, &super::downloads::collection_progress(ctx));
}

/// Reloads the library after a custom item changed. The open collection detail
/// (if any) is re-queried by [`reload`].
pub(super) fn refresh_after_change(ctx: &Rc<AppContext>) {
    reload(ctx);
}

/// Song count, total duration and download state of a collection's
/// members. The download state is the cards' own
/// ([`cider_offline_core::Storage::collection_downloads`]).
async fn members_info(
    storage: &cider_offline_core::Storage,
    filter: LibraryFilter,
    id: &str,
    format: DownloadFormat,
) -> Result<MembersInfo, StorageError> {
    let members = storage
        .tracks_page(detail_query(filter, id), 0, usize::MAX)
        .await?
        .items;
    let mut info = summarize_members(&members, format);
    if let Some(download) = storage
        .collection_downloads(filter, vec![id.to_owned()], format)
        .await?
        .remove(id)
    {
        info.download = download;
    }
    Ok(info)
}

/// Summarizes a collection's member tracks for the detail header: count,
/// length and how much is downloaded (each song once; a song is complete
/// when it misses no mix `format` wants, see
/// [`cider_offline_core::download_needs`]).
pub(super) fn summarize_members(members: &[LibraryTrack], format: DownloadFormat) -> MembersInfo {
    let total_ms = members
        .iter()
        .filter_map(|m| m.track.duration_ms)
        .map(u64::from)
        .sum();
    let mut seen = HashSet::new();
    let download = CollectionDownload::of(
        members
            .iter()
            .filter(|m| seen.insert(m.track.id.as_str()))
            // The members alone don't say which collection pins them; the
            // detail takes the links from `collection_downloads`.
            .map(|m| (&m.track, m.spatial_known, true)),
        format,
    );
    MembersInfo {
        songs: members.len(),
        total_ms,
        download,
    }
}

/// Stores the open collection's member summary and shows its meta line.
fn apply_members_info(ctx: &AppContext, info: MembersInfo) {
    let is_custom = {
        let mut open = ctx.library.open.borrow_mut();
        let Some(open) = open.as_mut() else { return };
        open.download = info.download;
        open.is_custom
    };
    ctx.with_ui(|ui| {
        ui.global::<LibraryState>()
            .set_detail_meta(format::collection_meta(info.songs, info.total_ms, is_custom).into());
    });
}

pub(super) fn track_row(track: Track, artwork: Option<Image>, has_mapping: bool) -> TrackRow {
    TrackRow {
        duration: format::duration(track.duration_ms).into(),
        has_audio: track.has_audio(),
        // Refined for the download format by `track_slots`.
        missing: !track.has_audio() && !track.is_custom,
        has_spatial: track.audio.get(AudioVariant::Spatial).is_some(),
        lyrics: format::lyrics(track.lyrics_source).into(),
        lyrics_quality: format::lyrics_quality(track.lyrics_source, track.lyrics_quality),
        has_artwork: artwork.is_some(),
        artwork: artwork.unwrap_or_default(),
        busy: false,
        is_custom: track.is_custom,
        has_mapping,
        // Populated from the live download map by `set_track_dl`.
        dl_status: slint::SharedString::default(),
        dl_progress: 0.0,
        track_number: track
            .track_number
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0),
        placeholder: false,
        album: track.album.unwrap_or_default().into(),
        artist: track.artist.into(),
        title: track.title.into(),
        // The queue keys download status by the catalog (adam) id; keep it on
        // the row so library tracks (whose `id` is the `i.…` library id) match.
        catalog_id: track
            .catalog_id
            .clone()
            .unwrap_or_else(|| track.id.clone())
            .into(),
        id: track.id.into(),
    }
}

fn collection_row(playlist: Playlist, artwork: Option<Image>) -> CollectionRow {
    let (kind_label, kind_key) = match playlist.kind {
        CollectionKind::Album => ("Album", "album"),
        CollectionKind::Playlist => ("Playlist", "playlist"),
    };
    let artist = playlist.artist.filter(|artist| !artist.is_empty());
    let mut subtitle = match (&playlist.kind, &artist) {
        (CollectionKind::Album, Some(artist)) => artist.clone(),
        _ => kind_label.to_owned(),
    };
    if playlist.is_custom {
        subtitle.push_str(", Custom");
    }
    CollectionRow {
        id: playlist.id.into(),
        name: playlist.name.into(),
        subtitle: subtitle.into(),
        artist: artist.unwrap_or_default().into(),
        has_artwork: artwork.is_some(),
        artwork: artwork.unwrap_or_default(),
        kind: kind_key.into(),
        is_custom: playlist.is_custom,
        // Populated from the live download aggregate by `set_collection_dl`.
        dl_status: slint::SharedString::default(),
        dl_progress: 0.0,
        // Populated from the files on disk by `apply_collection_download`.
        downloaded: false,
        partial: false,
        elsewhere: false,
        dl_done: 0,
        dl_total: 0,
        pinned: false,
        keep: false,
        placeholder: false,
    }
}

// ----- Covers ----------------------------------------------------------------

/// Queues covers wanted by visible rows; flushed once on the next event-loop
/// turn so one scroll step becomes one batch (and never re-enters a model
/// from inside its `row_data`).
fn queue_art(ctx: &Rc<AppContext>, requests: Vec<ArtRequest>) {
    ctx.library.art_queue.borrow_mut().extend(requests);
    if ctx.library.art_flush_scheduled.replace(true) {
        return;
    }
    let weak = Rc::downgrade(ctx);
    slint::Timer::single_shot(Duration::ZERO, move || {
        if let Some(ctx) = weak.upgrade() {
            flush_art(&ctx);
        }
    });
}

fn flush_art(ctx: &Rc<AppContext>) {
    ctx.library.art_flush_scheduled.set(false);
    let requests = std::mem::take(&mut *ctx.library.art_queue.borrow_mut());
    let (hits, misses) = ctx.library.images.borrow_mut().triage(requests);
    if !hits.is_empty() {
        apply_images(ctx, &hits);
    }
    for chunk in misses.chunks(ART_CHUNK) {
        load_art(ctx, chunk.to_vec());
    }
}

/// Resolves + decodes one batch on the runtime and applies it on arrival.
fn load_art(ctx: &Rc<AppContext>, requests: Vec<ArtRequest>) {
    let keys: Vec<ArtKey> = requests.iter().map(|r| r.key.clone()).collect();
    ctx.with_backend(
        move |backend| artwork::load(backend.library, requests),
        move |ctx, decoded| {
            let Some(decoded) = decoded else {
                // Proxy gone: let the rows ask again later.
                ctx.library.images.borrow_mut().abandon(&keys);
                ctx.library.tracks.forget_asked(&keys);
                ctx.library.collections.forget_asked(&keys);
                return;
            };
            let images = ctx.library.images.borrow_mut().accept(decoded);
            apply_images(ctx, &images);
        },
    );
}

/// Puts arrived covers on the rows / cards / detail header showing them.
fn apply_images(ctx: &Rc<AppContext>, images: &HashMap<ArtKey, Image>) {
    ctx.library.tracks.apply_images(images);
    ctx.library.collections.apply_images(images);
    let hero = ctx
        .library
        .open
        .borrow()
        .as_ref()
        .and_then(|open| open.hero.as_ref())
        .and_then(|hero| images.get(&hero.key))
        .cloned();
    if let Some(image) = hero {
        ctx.with_ui(|ui| {
            let state = ui.global::<LibraryState>();
            state.set_detail_artwork(image);
            state.set_detail_has_artwork(true);
        });
    }
}

// ----- Detail ----------------------------------------------------------------

/// Opens the detail sub-view for a collection: its members (in the
/// collection's order) are the track query, paged like any other list.
fn open_collection(ctx: &Rc<AppContext>, id: &str) {
    if id.is_empty() {
        return;
    }
    let data = &ctx.library;
    let meta = data.collections.find(|c| c.id.as_str() == id);
    let hero = data
        .collections
        .art_of(|c| c.id.as_str() == id)
        .map(|art| ArtRequest::new(art.url, Bucket::Hero));
    let cached_hero = hero
        .as_ref()
        .and_then(|hero| data.images.borrow_mut().get(&hero.key));
    *data.open.borrow_mut() = Some(OpenDetail {
        id: id.to_owned(),
        kind: meta
            .as_ref()
            .map(|m| m.kind.to_string())
            .unwrap_or_default(),
        name: meta
            .as_ref()
            .map(|m| m.name.to_string())
            .unwrap_or_default(),
        hero: hero.clone(),
        // The card's state until the members are read.
        download: meta.as_ref().map(row_download).unwrap_or_default(),
        is_custom: meta.as_ref().is_some_and(|m| m.is_custom),
    });
    ctx.with_ui(|ui| {
        let state = ui.global::<LibraryState>();
        state.set_collection(id.into());
        state.set_detail_open(true);
        state.set_detail_meta("".into());
        if let Some(meta) = &meta {
            state.set_detail_title(meta.name.clone());
            state.set_detail_subtitle(detail_subtitle(meta).into());
            state.set_detail_kind(meta.kind.clone());
            state.set_detail_is_custom(meta.is_custom);
        }
        // The grid image is a stand-in until the sharper header size decodes.
        let image = cached_hero.clone().or_else(|| {
            meta.as_ref()
                .filter(|m| m.has_artwork)
                .map(|m| m.artwork.clone())
        });
        state.set_detail_has_artwork(image.is_some());
        state.set_detail_artwork(image.unwrap_or_default());
    });
    if cached_hero.is_none()
        && let Some(hero) = hero
    {
        queue_art(ctx, vec![hero]);
    }
    requery_tracks(ctx);
    refresh_members_info(ctx, id.to_owned());
    update_detail_download(ctx, &super::downloads::collection_progress(ctx));
}

/// "Album by Artist", "Album" or "Playlist" for the detail header.
pub(super) fn detail_subtitle(meta: &CollectionRow) -> String {
    match (meta.kind.as_str(), meta.artist.as_str()) {
        ("playlist", _) => "Playlist".to_owned(),
        (_, "") => "Album".to_owned(),
        (_, artist) => format!("Album by {artist}"),
    }
}

/// Recomputes the open collection's song count, length and completeness.
fn refresh_members_info(ctx: &Rc<AppContext>, id: String) {
    let filter = ctx.library.source.get().filter();
    let format = ctx.config().download_format;
    let collection = id.clone();
    ctx.with_backend(
        move |backend| async move {
            members_info(&backend.storage, filter, &collection, format).await
        },
        move |ctx, result| {
            if let Some(Ok(info)) = result {
                let still_open = ctx
                    .library
                    .open
                    .borrow()
                    .as_ref()
                    .is_some_and(|open| open.id == id);
                if still_open {
                    apply_members_info(ctx, info);
                }
                update_detail_download(ctx, &super::downloads::collection_progress(ctx));
            }
        },
    );
}

/// Leaves the detail sub-view and restores the full track list.
fn close_collection(ctx: &Rc<AppContext>) {
    *ctx.library.open.borrow_mut() = None;
    ctx.with_ui(|ui| {
        let state = ui.global::<LibraryState>();
        state.set_detail_open(false);
        state.set_collection("".into());
    });
    requery_tracks(ctx);
}

/// Updates a loaded track row in place (only that row is notified).
fn update_track(ctx: &AppContext, id: &str, change: impl Fn(&mut TrackRow)) {
    ctx.library.tracks.update_where(|row| {
        if row.id == id {
            change(row);
            true
        } else {
            false
        }
    });
}

fn import_audio(ctx: &Rc<AppContext>, id: String) {
    let owned = Rc::clone(ctx);
    dialogs::pick_audio_file(ctx, "Choose the audio file for this track", move |file| {
        let ctx = owned;
        let Some(file) = file else { return };
        start_import(&ctx, id, file);
    });
}

fn start_import(ctx: &Rc<AppContext>, id: String, file: PathBuf) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    ctx.with_backend(
        move |backend| async move { backend.library.import_audio(&track_id, &file).await },
        move |ctx, result| {
            update_track(ctx, &id, |row| row.busy = false);
            match result {
                Some(Ok(_)) => {
                    update_track(ctx, &id, |row| row.has_audio = true);
                    show_message(ctx, "Audio imported.");
                }
                Some(Err(error)) => show_error(ctx, &format!("Import failed: {error}")),
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Maps a track id to a user-chosen local file (replaces the Mappings page).
fn map_audio(ctx: &Rc<AppContext>, id: String) {
    let owned = Rc::clone(ctx);
    dialogs::pick_audio_file(
        ctx,
        "Choose the local file to map to this track",
        move |file| {
            let ctx = owned;
            let Some(file) = file else { return };
            update_track(&ctx, &id, |row| row.busy = true);
            let track_id = id.clone();
            ctx.with_backend(
                move |backend| async move {
                    if !tokio::fs::try_exists(&file).await.unwrap_or(false) {
                        return Err(format!("{} does not exist", file.display()));
                    }
                    backend
                        .storage
                        .set_track_mapping(TrackMapping {
                            track_id,
                            file_path: file,
                        })
                        .await
                        .map_err(|e| e.to_string())
                },
                move |ctx, result| {
                    update_track(ctx, &id, |row| row.busy = false);
                    match result {
                        Some(Ok(())) => {
                            update_track(ctx, &id, |row| row.has_mapping = true);
                            show_message(ctx, "Local file mapped.");
                        }
                        Some(Err(error)) => {
                            show_error(ctx, &format!("Could not map file: {error}"));
                        }
                        None => show_error(ctx, "The proxy is not running."),
                    }
                },
            );
        },
    );
}

/// Clears a track's local-file mapping.
fn clear_mapping(ctx: &Rc<AppContext>, id: String) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    ctx.with_backend(
        move |backend| async move { backend.storage.delete_track_mapping(&track_id).await },
        move |ctx, result| {
            update_track(ctx, &id, |row| row.busy = false);
            match result {
                Some(Ok(_)) => {
                    update_track(ctx, &id, |row| row.has_mapping = false);
                    show_message(ctx, "Mapping cleared.");
                }
                Some(Err(error)) => show_error(ctx, &format!("Could not clear mapping: {error}")),
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Lets the user pick a local TTML/LRC file and imports it as the track's
/// lyrics (via [`cider_offline_core::Library::import_lyrics`]).
fn choose_lyrics_file(ctx: &Rc<AppContext>, id: String) {
    let owned = Rc::clone(ctx);
    dialogs::pick_lyrics_file(ctx, "Choose a TTML or LRC lyrics file", move |file| {
        let ctx = owned;
        let Some(file) = file else { return };
        update_track(&ctx, &id, |row| row.busy = true);
        let track_id = id.clone();
        ctx.with_backend(
            move |backend| async move { backend.library.import_lyrics(&track_id, &file).await },
            move |ctx, result| {
                update_track(ctx, &id, |row| row.busy = false);
                match result {
                    Some(Ok(source)) => {
                        update_track(ctx, &id, |row| {
                            row.lyrics = format::lyrics(source).into();
                        });
                        show_message(ctx, "Lyrics file imported.");
                    }
                    Some(Err(error)) => {
                        show_error(ctx, &format!("Could not import lyrics: {error}"));
                    }
                    None => show_error(ctx, "The proxy is not running."),
                }
            },
        );
    });
}

/// Moves `id` to `position` within the open custom collection, persists the
/// new order with `place_track_in_collection`, then re-queries the members.
fn reorder_track(ctx: &Rc<AppContext>, id: &str, position: i32) {
    let collection = ctx
        .library
        .open
        .borrow()
        .as_ref()
        .map(|open| open.id.clone())
        .unwrap_or_default();
    if collection.is_empty() {
        return;
    }
    let position = usize::try_from(position.max(0)).unwrap_or(0);
    let track_id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            backend
                .storage
                .place_track_in_collection(&collection, &track_id, position)
                .await
        },
        move |ctx, result| {
            match result {
                Some(Ok(())) => {}
                Some(Err(error)) => show_error(ctx, &format!("Could not reorder: {error}")),
                None => show_error(ctx, "The proxy is not running."),
            }
            // Show the authoritative order either way.
            requery_tracks(ctx);
        },
    );
}

fn refetch_lyrics(ctx: &Rc<AppContext>, id: String) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    ctx.with_backend(
        move |backend| async move { backend.library.refresh_lyrics(&track_id).await },
        move |ctx, result| {
            update_track(ctx, &id, |row| row.busy = false);
            match result {
                Some(Ok(source)) => {
                    update_track(ctx, &id, |row| row.lyrics = format::lyrics(source).into());
                    refresh_download_state(ctx);
                    show_message(
                        ctx,
                        match source {
                            LyricsSource::None => "No lyrics found.",
                            LyricsSource::Custom => {
                                "Kept your imported lyrics. Choose another lyrics file to replace them."
                            }
                            _ => "Lyrics updated.",
                        },
                    );
                }
                Some(Err(error)) => show_error(ctx, &format!("Lyrics lookup failed: {error}")),
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Removes a track. The page asks for confirmation before calling this.
fn remove_track(ctx: &Rc<AppContext>, id: String) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    ctx.with_backend(
        move |backend| async move { backend.library.remove_track(&track_id).await },
        move |ctx, result| match result {
            Some(Ok(())) => {
                ctx.with_ui(|ui| {
                    let state = ui.global::<LibraryState>();
                    let tracks = (state.get_total_tracks() - 1).max(0);
                    state.set_total_tracks(tracks);
                    state.set_count_line(
                        format::library_counts(
                            usize::try_from(state.get_total_collections()).unwrap_or(0),
                            usize::try_from(tracks).unwrap_or(0),
                        )
                        .into(),
                    );
                });
                if ctx.library.open.borrow().is_some() {
                    // Also refreshes the detail's song count and length.
                    reload(ctx);
                } else {
                    requery_tracks(ctx);
                }
                show_message(ctx, "Song removed.");
            }
            Some(Err(error)) => {
                update_track(ctx, &id, |row| row.busy = false);
                show_error(ctx, &format!("Could not remove the song: {error}"));
            }
            None => update_track(ctx, &id, |row| row.busy = false),
        },
    );
}

/// Download key (catalog id) of a stored track.
async fn download_key(
    storage: &cider_offline_core::Storage,
    id: &str,
) -> Result<Option<String>, StorageError> {
    Ok(storage
        .get_track(id)
        .await?
        .map(|track| track.catalog_id.unwrap_or(track.id)))
}

/// The download key of collection member `id`, or `None` for a custom song
/// (its audio is the user's own and never a download) or an unknown one.
async fn member_download_key(
    storage: &cider_offline_core::Storage,
    id: &str,
) -> Result<Option<String>, StorageError> {
    Ok(storage
        .get_track(id)
        .await?
        .filter(|track| !track.is_custom)
        .map(|track| track.catalog_id.unwrap_or(track.id)))
}

/// "Remove download" for one Apple Music song: deletes its downloaded audio
/// (lyrics and metadata stay). The page asks for confirmation first.
fn remove_download(ctx: &Rc<AppContext>, id: String) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    let with_lyrics = ctx.config().remove_lyrics_with_downloads;
    ctx.with_backend(
        move |backend| async move {
            let key = download_key(&backend.storage, &track_id)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or(track_id);
            let removed = backend
                .downloads
                .remove_downloads(None, vec![key])
                .await
                .map_err(|e| e.to_string())?;
            let lyrics = if with_lyrics {
                backend.library.remove_provider_lyrics(&removed).await
            } else {
                0
            };
            Ok::<_, String>((removed, lyrics))
        },
        move |ctx, result| {
            update_track(ctx, &id, |row| row.busy = false);
            match result {
                Some(Ok((removed, _))) if removed.is_empty() => {
                    show_message(ctx, "This song had no downloaded audio.");
                }
                Some(Ok((_, 0))) => show_message(ctx, "Download removed."),
                Some(Ok(_)) => {
                    refresh_track_lyrics(ctx, &id);
                    show_message(ctx, "Download and lyrics removed.");
                }
                Some(Err(error)) => {
                    show_error(ctx, &format!("Could not remove the download: {error}"));
                }
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// "Remove download" for a whole album / playlist, Apple Music style: drops
/// the collection's pin and deletes only songs no other downloaded collection
/// (or individual download) still needs.
fn remove_collection_download(ctx: &Rc<AppContext>, id: String) {
    if id.is_empty() {
        return;
    }
    let filter = ctx.library.source.get().filter();
    let collection = id.clone();
    let with_lyrics = ctx.config().remove_lyrics_with_downloads;
    ctx.with_backend(
        move |backend| async move {
            let keys = collection_keys(&backend.storage, filter, &collection).await?;
            // Songs another download still needs keep their files.
            let kept = backend
                .storage
                .download_removal_preview(&collection, keys.clone())
                .await
                .map_err(|e| e.to_string())?
                .kept;
            let kept = NamedSongs::of(&backend.storage, &kept).await;
            let removed = backend
                .downloads
                .remove_downloads(Some(&collection), keys)
                .await
                .map_err(|e| e.to_string())?;
            if with_lyrics {
                backend.library.remove_provider_lyrics(&removed).await;
            }
            let removed = NamedSongs::of(&backend.storage, &removed).await;
            Ok::<_, String>((removed, kept))
        },
        move |ctx, result| match result {
            Some(Ok((removed, kept))) => {
                ctx.library.pinned.borrow_mut().remove(&id);
                show_message(ctx, &removed_collection_message(&removed, &kept));
                reload(ctx);
            }
            Some(Err(error)) => show_error(ctx, &format!("Could not remove the download: {error}")),
            None => show_error(ctx, "The proxy is not running."),
        },
    );
}

/// Records that collection `id` (seen through `filter`) was downloaded, so
/// its pins can be checked against its songs later ("unlinked downloads")
/// and "Keep downloaded" can find it. `keep = None` leaves that flag as is.
async fn register_collection(
    storage: &cider_offline_core::Storage,
    id: &str,
    filter: LibraryFilter,
    kind: DownloadSourceKind,
    name: &str,
    keep: Option<bool>,
) -> Result<(), String> {
    storage
        .register_download_collection(
            DownloadCollection {
                owner: id.to_owned(),
                filter,
                kind,
                name: name.to_owned(),
                keep: false,
            },
            keep,
        )
        .await
        .map_err(|e| format!("Could not record the download: {e}"))
}

/// Download keys (catalog ids) of every song of collection `id`.
pub(super) async fn collection_keys(
    storage: &cider_offline_core::Storage,
    filter: LibraryFilter,
    id: &str,
) -> Result<Vec<String>, String> {
    let members = storage
        .collection_members(filter, id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let mut keys = Vec::with_capacity(members.len());
    for member in members {
        if let Some(key) = member_download_key(storage, &member)
            .await
            .map_err(|e| e.to_string())?
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// Songs a dialog or message talks about: how many, and their titles when
/// there are few enough to name (see [`format::songs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct NamedSongs {
    /// How many songs.
    pub count: usize,
    /// Their titles, or empty when they are counted instead.
    pub titles: Vec<String>,
}

impl NamedSongs {
    /// The songs `keys` (download keys), with their titles when a message
    /// can name them all.
    pub(super) async fn of(storage: &cider_offline_core::Storage, keys: &[String]) -> Self {
        let mut titles = Vec::new();
        if keys.len() <= format::MAX_NAMED_SONGS {
            for key in keys {
                let Ok(Some(track)) = storage.get_track(key).await else {
                    // A song that cannot be named is counted, with the rest.
                    titles.clear();
                    break;
                };
                titles.push(track.title);
            }
        }
        Self {
            count: keys.len(),
            titles,
        }
    }

    /// "“A” and “B”" or "37 songs".
    fn phrase(&self) -> String {
        format::songs(self.count, &self.titles)
    }
}

/// The confirmation text of removing a collection's downloads: which songs
/// lose their files and which shared ones are kept (named up to
/// [`format::MAX_NAMED_SONGS`], counted beyond).
pub(super) fn remove_collection_message(
    name: &str,
    deleted: &NamedSongs,
    kept: &NamedSongs,
) -> String {
    let what = if name.is_empty() {
        "this collection".to_owned()
    } else {
        format!("“{name}”")
    };
    let mut text = match deleted.count {
        0 => format!("Remove the download of {what}? No downloaded file is deleted."),
        n => format!(
            "Remove the downloads of {what}? {} {} deleted from this computer.",
            deleted.phrase(),
            format::is_are(n)
        ),
    };
    if kept.count > 0 {
        let _ = write!(
            text,
            " {} {} kept because another album, playlist or single download still uses {}.",
            kept.phrase(),
            format::is_are(kept.count),
            if kept.count == 1 { "it" } else { "them" }
        );
    }
    text.push_str(" Lyrics and song details stay.");
    text
}

/// The message after removing a collection's downloads.
pub(super) fn removed_collection_message(removed: &NamedSongs, kept: &NamedSongs) -> String {
    match (removed.count, kept.count) {
        (0, 0) => "Download removed.".to_owned(),
        (0, _) => format!(
            "Download removed. {} {} kept because other downloads still need {}.",
            kept.phrase(),
            format::is_are(kept.count),
            if kept.count == 1 { "it" } else { "them" }
        ),
        (_, 0) => format!("Removed the downloads of {}.", removed.phrase()),
        _ => format!(
            "Removed the downloads of {}. {} {} kept because other downloads still need {}.",
            removed.phrase(),
            kept.phrase(),
            format::is_are(kept.count),
            if kept.count == 1 { "it" } else { "them" }
        ),
    }
}
/// "Remove downloads…" for a collection: counts what would be deleted and
/// what is shared, asks, then removes.
fn confirm_remove_collection_download(ctx: &Rc<AppContext>, id: String) {
    if id.is_empty() {
        return;
    }
    let filter = ctx.library.source.get().filter();
    let (_, name) = collection_source(ctx, &id);
    let collection = id.clone();
    ctx.with_backend(
        move |backend| async move {
            let keys = collection_keys(&backend.storage, filter, &collection).await?;
            let preview = backend
                .storage
                .download_removal_preview(&collection, keys)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((
                NamedSongs::of(&backend.storage, &preview.deleted).await,
                NamedSongs::of(&backend.storage, &preview.kept).await,
            ))
        },
        move |ctx, result| match result {
            Some(Ok((deleted, kept))) => {
                let message = remove_collection_message(&name, &deleted, &kept);
                let c = Rc::clone(ctx);
                dialogs::confirm(
                    ctx,
                    dialogs::Prompt {
                        title: "Remove downloads",
                        message: &message,
                        confirm: "Remove downloads",
                        destructive: true,
                    },
                    move || remove_collection_download(&c, id),
                );
            }
            Some(Err(error)) => show_error(ctx, &format!("Could not read the download: {error}")),
            None => show_error(ctx, "The proxy is not running."),
        },
    );
}

/// Pause / resume / retry / cancel the queued downloads of a collection's
/// songs. Cancel asks first; downloaded files are never deleted by it.
fn collection_download_action(ctx: &Rc<AppContext>, id: &str, action: &str) {
    if id.is_empty() {
        return;
    }
    let action = match action {
        "pause" => QueueAction::Pause,
        "resume" => QueueAction::Resume,
        "retry" => QueueAction::Retry,
        "cancel" => QueueAction::Cancel,
        _ => return,
    };
    let filter = ctx.library.source.get().filter();
    let collection = id.to_owned();
    ctx.with_backend(
        move |backend| async move { collection_keys(&backend.storage, filter, &collection).await },
        move |ctx, result| {
            let keys = match result {
                Some(Ok(keys)) => keys,
                Some(Err(error)) => {
                    show_error(ctx, &format!("Could not load the collection: {error}"));
                    return;
                }
                None => return,
            };
            // Only rows the queue holds for these songs, in a state the
            // action applies to.
            let rows = super::downloads::rows_for(ctx, &keys, action);
            if rows.is_empty() {
                return;
            }
            if action == QueueAction::Cancel {
                let titles = super::downloads::row_titles(ctx, &rows);
                let message = super::downloads::cancel_message(&titles, false);
                let c = Rc::clone(ctx);
                dialogs::confirm(
                    ctx,
                    dialogs::Prompt {
                        title: "Cancel downloads",
                        message: &message,
                        confirm: "Cancel downloads",
                        destructive: true,
                    },
                    move || super::downloads::apply(&c, action, rows),
                );
            } else {
                super::downloads::apply(ctx, action, rows);
            }
        },
    );
}

/// "Keep downloaded" on (registers the collection as kept and downloads its
/// missing songs) or off (songs added later are no longer downloaded; what
/// is downloaded stays).
fn set_keep_downloaded(ctx: &Rc<AppContext>, id: &str, keep: bool) {
    if id.is_empty() {
        return;
    }
    if keep {
        ctx.library.kept.borrow_mut().insert(id.to_owned());
        download_collection_with(ctx, id, Some(true));
        return;
    }
    ctx.library.kept.borrow_mut().remove(id);
    sync_download_status(ctx);
    let (source_kind, name) = collection_source(ctx, id);
    let filter = ctx.library.source.get().filter();
    let collection = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            register_collection(
                &backend.storage,
                &collection,
                filter,
                source_kind,
                &name,
                Some(false),
            )
            .await
        },
        |ctx, result| match result {
            Some(Ok(())) => show_message(ctx, "Songs added later are no longer downloaded."),
            Some(Err(message)) => show_error(ctx, &message),
            None => show_error(ctx, "The proxy is not running."),
        },
    );
}

/// Reflects downloaded / removed audio on the loaded song rows at once
/// (`keys` are download keys, i.e. catalog ids).
pub(super) fn mark_audio(ctx: &Rc<AppContext>, keys: &[String], has_audio: bool) {
    if keys.is_empty() {
        return;
    }
    ctx.library.tracks.update_where(|row| {
        let hit = keys.iter().any(|key| key == row.catalog_id.as_str());
        let changed = hit
            && (row.has_audio != has_audio
                || row.missing == has_audio
                || (!has_audio && row.has_spatial));
        if changed {
            row.has_audio = has_audio;
            // A finished download fetched every mix the format allows.
            row.missing = !has_audio;
            if !has_audio {
                row.has_spatial = false;
            }
        }
        changed
    });
    // Collections may have gained or lost files.
    refresh_download_state(ctx);
    if !has_audio {
        refresh_pins(ctx);
    }
}

/// Shows which mixes the songs named by `keys` (download keys) have on disk,
/// as read by [`super::downloads::refresh_variants`]: `found` holds the
/// variants of the keys that have any.
pub(super) fn apply_variants(
    ctx: &Rc<AppContext>,
    keys: &[String],
    found: &HashMap<String, Vec<AudioVariant>>,
) {
    ctx.library.tracks.update_where(|row| {
        let key = row.catalog_id.as_str();
        if !keys.iter().any(|k| k == key) {
            return false;
        }
        // Only the spatial badge: `has_audio` follows the download events
        // (a mapping counts as a stereo mix here but is not a download).
        let has_spatial = found
            .get(key)
            .is_some_and(|v| v.contains(&AudioVariant::Spatial));
        let changed = row.has_spatial != has_spatial;
        row.has_spatial = has_spatial;
        changed
    });
}

/// Ids of the collections set to "Keep downloaded".
async fn kept_collections(
    storage: &cider_offline_core::Storage,
) -> Result<Vec<String>, StorageError> {
    Ok(storage
        .download_collections()
        .await?
        .into_iter()
        .filter(|collection| collection.keep)
        .map(|collection| collection.owner)
        .collect())
}

/// Re-reads which collections are pinned or kept and updates the cards /
/// detail.
pub(super) fn refresh_pins(ctx: &Rc<AppContext>) {
    ctx.with_backend(
        |backend| async move {
            let pinned = backend.storage.pinned_collections().await?;
            let kept = kept_collections(&backend.storage).await?;
            Ok::<_, StorageError>((pinned, kept))
        },
        |ctx, result| {
            if let Some(Ok((pinned, kept))) = result {
                *ctx.library.pinned.borrow_mut() = pinned.into_iter().collect();
                *ctx.library.kept.borrow_mut() = kept.into_iter().collect();
                sync_download_status(ctx);
            }
        },
    );
}

fn fetch_missing_lyrics(ctx: &Rc<AppContext>) {
    ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(true));
    show_message(ctx, "Looking up missing lyrics…");
    let (progress, finished) = lyrics_progress(ctx);
    ctx.with_backend(
        move |backend| async move {
            backend
                .library
                .fetch_missing_lyrics_with(&move |p| {
                    let _ = progress.send(*p);
                })
                .await
        },
        move |ctx, result| {
            finished.set(true);
            ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(false));
            match result {
                Some(Ok(report)) => {
                    show_lyrics_report(ctx, report);
                    reload(ctx);
                }
                Some(Err(error)) => show_error(ctx, &format!("Lyrics lookup failed: {error}")),
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Fetches lyrics for every track of one collection through the batch
/// lookup ([`cider_offline_core::Library::fetch_lyrics_for_with`]): the
/// configured providers, order and failure policy apply, and songs that
/// already have lyrics keep them unless "Overwrite lyrics" is on. Member ids
/// come from the core's grouped view, so synthesized album groups resolve
/// too.
fn fetch_collection_lyrics(ctx: &Rc<AppContext>, id: &str) {
    if id.is_empty() {
        return;
    }
    let filter = ctx.library.source.get().filter();
    let collection = id.to_owned();
    ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(true));
    show_message(ctx, "Looking up lyrics for this collection…");
    let (progress, finished) = lyrics_progress(ctx);
    ctx.with_backend(
        move |backend| async move {
            let ids = backend
                .storage
                .collection_members(filter, &collection)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            if ids.is_empty() {
                return Err("This collection has no tracks.".to_owned());
            }
            Ok::<LyricsReport, String>(
                backend
                    .library
                    .fetch_lyrics_for_with(&ids, &move |p| {
                        let _ = progress.send(*p);
                    })
                    .await,
            )
        },
        move |ctx, result| {
            finished.set(true);
            ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(false));
            match result {
                Some(Ok(report)) => {
                    show_lyrics_report(ctx, report);
                    reload(ctx);
                }
                Some(Err(message)) => {
                    show_error(ctx, &format!("Lyrics lookup failed: {message}"));
                }
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// "Remove lyrics" of one song: deletes all of its local lyrics, the
/// lyrics the user imported included (the user removes them on this song
/// explicitly). Asks first when the song has imported lyrics.
fn remove_song_lyrics(ctx: &Rc<AppContext>, id: String) {
    let custom = ctx
        .library
        .tracks
        .find(|row| row.id.as_str() == id)
        .is_some_and(|row| row.lyrics.as_str() == format::lyrics(LyricsSource::Custom));
    if !custom {
        run_remove_song_lyrics(ctx, id);
        return;
    }
    let c = Rc::clone(ctx);
    dialogs::confirm(
        ctx,
        dialogs::Prompt {
            title: "Remove lyrics",
            message: "Remove this song's lyrics? The lyrics file you imported is deleted too.",
            confirm: "Remove",
            destructive: true,
        },
        move || run_remove_song_lyrics(&c, id),
    );
}

fn run_remove_song_lyrics(ctx: &Rc<AppContext>, id: String) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    ctx.with_backend(
        move |backend| async move { backend.library.remove_lyrics(&track_id, true).await },
        move |ctx, result| {
            update_track(ctx, &id, |row| row.busy = false);
            match result {
                Some(Ok(true)) => {
                    refresh_track_lyrics(ctx, &id);
                    show_message(ctx, "Lyrics removed.");
                }
                Some(Ok(false)) => show_message(ctx, "This song had no lyrics."),
                Some(Err(error)) => {
                    show_error(ctx, &format!("Could not remove the lyrics: {error}"));
                }
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Re-reads one song's lyrics state onto its loaded row.
fn refresh_track_lyrics(ctx: &Rc<AppContext>, id: &str) {
    let track_id = id.to_owned();
    let row_id = id.to_owned();
    ctx.with_backend(
        move |backend| async move { backend.storage.get_track(&track_id).await },
        move |ctx, result| {
            if let Some(Ok(Some(track))) = result {
                update_track(ctx, &row_id, |row| {
                    row.lyrics = format::lyrics(track.lyrics_source).into();
                    row.lyrics_quality =
                        format::lyrics_quality(track.lyrics_source, track.lyrics_quality);
                });
                // Lyrics never count towards a download, but its state is
                // read again so nothing shown can lag behind a change.
                refresh_download_state(ctx);
            }
        },
    );
}

/// The confirmation text of removing a collection's lyrics.
pub(super) fn remove_collection_lyrics_message(name: &str) -> String {
    let what = if name.is_empty() {
        "this collection".to_owned()
    } else {
        format!("“{name}”")
    };
    format!(
        "Remove the lyrics of every song in {what}? Lyrics from Apple and lyrics lookups are \
         deleted from this computer; lyrics you imported are kept. You can fetch them again later."
    )
}

/// "Remove lyrics…" for a collection: asks, then removes the fetched lyrics
/// of its songs. Lyrics the user imported are never removed here.
fn confirm_remove_collection_lyrics(ctx: &Rc<AppContext>, id: String) {
    if id.is_empty() {
        return;
    }
    let (_, name) = collection_source(ctx, &id);
    let message = remove_collection_lyrics_message(&name);
    let c = Rc::clone(ctx);
    dialogs::confirm(
        ctx,
        dialogs::Prompt {
            title: "Remove lyrics",
            message: &message,
            confirm: "Remove lyrics",
            destructive: true,
        },
        move || remove_collection_lyrics(&c, id),
    );
}

fn remove_collection_lyrics(ctx: &Rc<AppContext>, id: String) {
    let filter = ctx.library.source.get().filter();
    ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(true));
    ctx.with_backend(
        move |backend| async move {
            let ids = backend
                .storage
                .collection_members(filter, &id)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            Ok::<usize, String>(backend.library.remove_provider_lyrics(&ids).await)
        },
        |ctx, result| {
            ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(false));
            match result {
                Some(Ok(0)) => show_message(ctx, "No song here had lyrics to remove."),
                Some(Ok(1)) => {
                    show_message(ctx, "Removed the lyrics of 1 song.");
                    reload(ctx);
                }
                Some(Ok(count)) => {
                    show_message(ctx, &format!("Removed the lyrics of {count} songs."));
                    reload(ctx);
                }
                Some(Err(error)) => {
                    show_error(ctx, &format!("Could not remove the lyrics: {error}"));
                }
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Whether LRCLIB is the only enabled lyrics provider ("not on LRCLIB").
fn lrclib_only(ctx: &AppContext) -> bool {
    let config = ctx.config();
    let mut enabled = config.lyrics.enabled_providers();
    enabled.next() == Some(LyricsProviderId::Lrclib) && enabled.next().is_none()
}

/// A sender for batch progress and the flag that stops showing it: each
/// update becomes the toast text until the batch has finished.
fn lyrics_progress(
    ctx: &Rc<AppContext>,
) -> (
    tokio::sync::mpsc::UnboundedSender<LyricsProgress>,
    Rc<Cell<bool>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LyricsProgress>();
    let finished = Rc::new(Cell::new(false));
    let done = Rc::clone(&finished);
    let c = Rc::clone(ctx);
    let lrclib_only = lrclib_only(ctx);
    let spawned = slint::spawn_local(async move {
        while let Some(progress) = rx.recv().await {
            if done.get() {
                break;
            }
            show_message(&c, &format::lyrics_progress(&progress, lrclib_only));
        }
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not show lyrics progress");
    }
    (tx, finished)
}

/// Toast for a bulk lyrics lookup; an error toast when lookups failed.
fn show_lyrics_report(ctx: &AppContext, report: LyricsReport) {
    let text = format::lyrics_report(&report, lrclib_only(ctx));
    if report.failed > 0 {
        show_error(ctx, &text);
    } else {
        show_message(ctx, &text);
    }
}

/// Builds a queued single-song [`DownloadRecord`] from a cached library
/// track, keyed on its catalog id (the DRM downloader's webPlayback needs the
/// numeric catalog id, not the `i.…` library id).
/// A track's download key: its catalog id, else its own id.
fn download_key_of(track: &Track) -> String {
    track.catalog_id.clone().unwrap_or_else(|| track.id.clone())
}

fn track_to_download_record(track: &Track) -> DownloadRecord {
    record_from_track(track, None)
}

fn download_track(ctx: &Rc<AppContext>, id: String) {
    update_track(ctx, &id, |row| row.busy = true);
    let track_id = id.clone();
    ctx.with_backend(
        move |backend| async move {
            let track = backend
                .storage
                .get_track(&track_id)
                .await
                .map_err(|e| format!("Could not load track: {e}"))?
                .ok_or_else(|| "Track not found in the library.".to_owned())?;
            let record = track_to_download_record(&track);
            backend
                .downloads
                .enqueue(vec![record])
                .map_err(|e| format!("Could not enqueue: {e}"))?;
            Ok::<(), String>(())
        },
        move |ctx, result| {
            update_track(ctx, &id, |row| row.busy = false);
            match result {
                Some(Ok(())) => show_message(ctx, "Added to download queue."),
                Some(Err(message)) => show_error(ctx, &format!("Download failed: {message}")),
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

fn download_missing_audio(ctx: &Rc<AppContext>) {
    ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(true));
    show_message(ctx, "Queuing missing audio downloads…");
    let format = ctx.config().download_format;
    ctx.with_backend(
        move |backend| async move {
            let tracks = backend
                .storage
                .list_tracks()
                .await
                .map_err(|e| format!("Could not list tracks: {e}"))?;
            let known = backend
                .storage
                .song_spatial()
                .await
                .map_err(|e| format!("Could not list tracks: {e}"))?;
            let records: Vec<DownloadRecord> = tracks
                .into_iter()
                .filter(|t| {
                    track_needs_download(t, format, known.get(&download_key_of(t)).copied())
                })
                .map(|track| track_to_download_record(&track))
                .collect();
            let count = records.len();
            if count > 0 {
                backend
                    .downloads
                    .enqueue(records)
                    .map_err(|e| format!("Could not enqueue: {e}"))?;
            }
            Ok::<usize, String>(count)
        },
        |ctx, result| {
            ctx.with_ui(|ui| ui.global::<LibraryState>().set_bulk_busy(false));
            match result {
                Some(Ok(0)) => show_message(ctx, "No tracks are missing audio."),
                Some(Ok(count)) => {
                    show_message(ctx, &format!("Added {count} tracks to the download queue."));
                }
                Some(Err(message)) => show_error(ctx, &format!("Failed: {message}")),
                None => show_error(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Enqueues every not-yet-downloaded track of a collection as a grouped
/// download, reusing the same queue path as the per-song and Downloads-drawer
/// actions. Records are tagged with the collection's source id / name so the
/// inline card + detail indicators can aggregate their progress.
fn download_collection(ctx: &Rc<AppContext>, id: &str) {
    download_collection_with(ctx, id, None);
}

/// [`download_collection`], also setting "Keep downloaded" when `keep` is
/// given.
fn download_collection_with(ctx: &Rc<AppContext>, id: &str, keep: Option<bool>) {
    if id.is_empty() {
        return;
    }
    let (source_kind, name) = collection_source(ctx, id);
    let label = match source_kind {
        DownloadSourceKind::Playlist => "playlist",
        _ => "album",
    };
    let filter = ctx.library.source.get().filter();
    let format = ctx.config().download_format;
    let collection_id = id.to_owned();
    let pin_id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            // Member ids from the core's grouped view resolve synthesized
            // album groups too.
            let ids = backend
                .storage
                .collection_members(filter, &collection_id)
                .await
                .map_err(|e| format!("Could not load the collection: {e}"))?
                .unwrap_or_default();
            if ids.is_empty() {
                return Err("This collection has no tracks to download.".to_owned());
            }
            let known = backend
                .storage
                .song_spatial()
                .await
                .map_err(|e| format!("Could not load the collection: {e}"))?;
            let mut tracks = Vec::with_capacity(ids.len());
            for track_id in ids {
                if let Some(track) = backend
                    .storage
                    .get_track(&track_id)
                    .await
                    .map_err(|e| format!("Could not load track: {e}"))?
                {
                    tracks.push(track);
                }
            }
            let plan = CollectionPlan::of(&tracks, format, &known);
            // The collection now owns its songs' downloads, including those
            // already on disk through other downloads, so they stay while it
            // wants them and "Remove download" can release them later.
            let linked = backend
                .storage
                .link_download_collection(
                    DownloadCollection {
                        owner: collection_id.clone(),
                        filter,
                        kind: source_kind,
                        name: name.clone(),
                        keep: false,
                    },
                    keep,
                    plan.keys,
                )
                .await
                .map_err(|e| format!("Could not record the download: {e}"))?;
            let records: Vec<DownloadRecord> = plan
                .missing
                .into_iter()
                .map(|track| {
                    let mut record = track_to_download_record(track);
                    record.source_kind = source_kind;
                    record.source_id = Some(collection_id.clone());
                    record.source_name = (!name.is_empty()).then(|| name.clone());
                    record
                })
                .collect();
            let queued = records.len();
            if queued > 0 {
                backend
                    .downloads
                    .enqueue(records)
                    .map_err(|e| format!("Could not enqueue: {e}"))?;
            }
            Ok::<_, String>((queued, linked))
        },
        move |ctx, result| match result {
            Some(Ok((queued, linked))) => {
                ctx.library.pinned.borrow_mut().insert(pin_id);
                refresh_pins(ctx);
                refresh_download_state(ctx);
                show_message(ctx, &download_collection_message(queued, linked, label));
            }
            Some(Err(message)) => show_error(ctx, &message),
            None => show_error(ctx, "The proxy is not running."),
        },
    );
}

/// What downloading a collection does with its songs.
#[derive(Debug, Default)]
struct CollectionPlan<'a> {
    /// The download key of every song, which the collection pins (songs
    /// already on disk through other downloads included).
    keys: Vec<String>,
    /// The songs missing a mix the format wants, to be queued.
    missing: Vec<&'a Track>,
}

impl<'a> CollectionPlan<'a> {
    /// The plan for the collection's songs `tracks` under `format`; `known`
    /// is what the queue learned about spatial mixes, by download key.
    fn of(tracks: &'a [Track], format: DownloadFormat, known: &HashMap<String, bool>) -> Self {
        let mut plan = Self::default();
        for track in tracks {
            let key = download_key_of(track);
            if track_needs_download(track, format, known.get(&key).copied()) {
                plan.missing.push(track);
            }
            if !plan.keys.contains(&key) {
                plan.keys.push(key);
            }
        }
        plan
    }
}

/// The toast after downloading collection (`label`: "album" or "playlist"):
/// `queued` songs were queued and `linked` songs newly linked to it.
fn download_collection_message(queued: usize, linked: usize, label: &str) -> String {
    match (queued, linked) {
        (0, 0) => format!("Every song in this {label} is already downloaded."),
        (0, 1) => format!("Linked 1 song to this {label}. Nothing had to be downloaded."),
        (0, n) => format!("Linked {n} songs to this {label}. Nothing had to be downloaded."),
        (1, _) => "Added 1 song to the download queue.".to_owned(),
        (n, _) => format!("Added {n} songs to the download queue."),
    }
}

/// The download source kind and name of collection `id` (a loaded card or
/// the open detail), for tagging its queued songs.
fn collection_source(ctx: &AppContext, id: &str) -> (DownloadSourceKind, String) {
    let (kind_key, name) = ctx
        .library
        .collections
        .find(|c| c.id.as_str() == id)
        .map(|c| (c.kind.to_string(), c.name.to_string()))
        .or_else(|| {
            ctx.library
                .open
                .borrow()
                .as_ref()
                .filter(|open| open.id == id)
                .map(|open| (open.kind.clone(), open.name.clone()))
        })
        .unwrap_or_default();
    let source_kind = if kind_key == "playlist" {
        DownloadSourceKind::Playlist
    } else {
        DownloadSourceKind::Album
    };
    (source_kind, name)
}

/// The confirmation text of a re-download of `count` songs.
fn redownload_message(count: usize) -> String {
    let songs = if count == 1 {
        "1 song".to_owned()
    } else {
        format!("{count} songs")
    };
    format!("Re-download {songs}? Existing files are replaced when the new ones finish.")
}

/// Asks to re-download `records`, then queues them. The queue writes each
/// new file next to the old one and swaps it in only once it is complete, so
/// the old file keeps playing until then; lyrics and links to local files
/// are not touched. `pin` is the collection that owns the downloads, if any.
fn confirm_redownload(ctx: &Rc<AppContext>, records: Vec<DownloadRecord>, pin: Option<String>) {
    if records.is_empty() {
        show_message(ctx, "There is nothing to download again here.");
        return;
    }
    let message = redownload_message(records.len());
    let c = Rc::clone(ctx);
    dialogs::confirm(
        ctx,
        dialogs::Prompt {
            title: "Re-download",
            message: &message,
            confirm: "Re-download",
            destructive: false,
        },
        move || {
            let count = records.len();
            let pin_id = pin.clone();
            c.with_backend(
                move |backend| async move {
                    if let Some(owner) = &pin {
                        let keys = records.iter().map(|r| r.track_id.clone()).collect();
                        backend
                            .storage
                            .pin_downloads(Some(owner), keys)
                            .await
                            .map_err(|e| format!("Could not record the download: {e}"))?;
                    }
                    backend
                        .downloads
                        .enqueue(records)
                        .map_err(|e| format!("Could not enqueue: {e}"))
                },
                move |ctx, result| match result {
                    Some(Ok(())) => {
                        if let Some(pin) = pin_id {
                            ctx.library.pinned.borrow_mut().insert(pin);
                        }
                        sync_download_status(ctx);
                        show_message(
                            ctx,
                            &format!("Added {count} track(s) to the download queue."),
                        );
                    }
                    Some(Err(message)) => show_error(ctx, &message),
                    None => show_error(ctx, "The proxy is not running."),
                },
            );
        },
    );
}

/// "Re-download" for one song: fetches it again in the current download
/// format (asks first).
fn redownload_track(ctx: &Rc<AppContext>, id: String) {
    ctx.with_backend(
        move |backend| async move {
            let track = backend
                .storage
                .get_track(&id)
                .await
                .map_err(|e| format!("Could not load track: {e}"))?
                .ok_or_else(|| "Track not found in the library.".to_owned())?;
            if track.is_custom {
                return Err("Custom songs have no Apple Music download.".to_owned());
            }
            let mut record = track_to_download_record(&track);
            record.replace = true;
            Ok::<_, String>(record)
        },
        |ctx, result| match result {
            Some(Ok(record)) => confirm_redownload(ctx, vec![record], None),
            Some(Err(message)) => show_error(ctx, &message),
            None => show_error(ctx, "The proxy is not running."),
        },
    );
}

/// "Re-download" for an album / playlist: fetches every Apple Music song of
/// it again in the current download format (asks first). Custom songs are
/// skipped.
fn redownload_collection(ctx: &Rc<AppContext>, id: &str) {
    if id.is_empty() {
        return;
    }
    let (source_kind, name) = collection_source(ctx, id);
    let filter = ctx.library.source.get().filter();
    let collection_id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            let ids = backend
                .storage
                .collection_members(filter, &collection_id)
                .await
                .map_err(|e| format!("Could not load the collection: {e}"))?
                .unwrap_or_default();
            let mut records = Vec::with_capacity(ids.len());
            for track_id in ids {
                let Some(track) = backend
                    .storage
                    .get_track(&track_id)
                    .await
                    .map_err(|e| format!("Could not load track: {e}"))?
                else {
                    continue;
                };
                if track.is_custom {
                    continue;
                }
                let mut record =
                    record_from_track(&track, Some((source_kind, &collection_id, &name)));
                record.replace = true;
                records.push(record);
            }
            register_collection(
                &backend.storage,
                &collection_id,
                filter,
                source_kind,
                &name,
                None,
            )
            .await?;
            Ok::<_, String>((records, collection_id))
        },
        |ctx, result| match result {
            Some(Ok((records, collection_id))) => {
                confirm_redownload(ctx, records, Some(collection_id));
            }
            Some(Err(message)) => show_error(ctx, &message),
            None => show_error(ctx, "The proxy is not running."),
        },
    );
}

/// Reflects the live download queue onto the loaded track and collection
/// rows (and the open detail), so each item shows its own inline status like
/// Apple Music. Only rows whose state changed are notified. Driven by every
/// download event via [`AppContext::handle`].
pub(super) fn sync_download_status(ctx: &Rc<AppContext>) {
    let tracks = super::downloads::track_progress(ctx);
    let collections = super::downloads::collection_progress(ctx);
    let data = &ctx.library;
    data.tracks.update_where(|row| set_track_dl(row, &tracks));
    let pinned = data.pinned.borrow();
    let kept = data.kept.borrow();
    data.collections.update_where(|row| {
        let is_pinned = pinned.contains(row.id.as_str());
        let is_kept = kept.contains(row.id.as_str());
        let pin_changed = row.pinned != is_pinned || row.keep != is_kept;
        row.pinned = is_pinned;
        row.keep = is_kept;
        set_collection_dl(row, &collections) || pin_changed
    });
    drop(kept);
    drop(pinned);
    update_detail_download(ctx, &collections);
}

/// Copies the live per-track download state onto a row; returns whether it
/// changed.
fn set_track_dl(row: &mut TrackRow, map: &HashMap<String, (String, f32)>) -> bool {
    // The queue keys status by the catalog id, so match on the row's catalog id
    // (not its library `i.…` id).
    let (status, progress) = map
        .get(row.catalog_id.as_str())
        .map_or(("", 0.0), |(s, p)| (s.as_str(), *p));
    let changed =
        row.dl_status.as_str() != status || (row.dl_progress - progress).abs() > f32::EPSILON;
    if changed {
        row.dl_status = status.into();
        row.dl_progress = progress;
    }
    changed
}

/// Copies the live aggregate queue state (in flight, paused, failed) onto a
/// collection row. Whether it is downloaded comes from its files only, see
/// [`apply_collection_download`].
fn set_collection_dl(row: &mut CollectionRow, map: &HashMap<String, (String, f32)>) -> bool {
    let (status, progress) = map
        .get(row.id.as_str())
        .map_or(("", 0.0), |(s, p)| (s.as_str(), *p));
    let changed =
        row.dl_status.as_str() != status || (row.dl_progress - progress).abs() > f32::EPSILON;
    if changed {
        row.dl_status = status.into();
        row.dl_progress = progress;
    }
    changed
}

/// The card and detail fields of a collection's download summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownloadFields {
    /// Every song complete, and the collection pins them all.
    downloaded: bool,
    /// Some songs have files, not all of them every wanted mix.
    partial: bool,
    /// Every song complete, some only through other downloads.
    elsewhere: bool,
    /// Complete songs, of `total`.
    done: i32,
    total: i32,
}

impl DownloadFields {
    fn of(summary: CollectionDownload) -> Self {
        let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
        let state = summary.state();
        Self {
            downloaded: state == CollectionDownloadState::Downloaded,
            partial: state == CollectionDownloadState::Partial,
            elsewhere: state == CollectionDownloadState::SavedElsewhere,
            done: count(summary.complete),
            total: count(summary.songs),
        }
    }
}

/// Shows a collection's download summary on its card; returns whether the
/// card changed.
pub(super) fn apply_collection_download(
    row: &mut CollectionRow,
    summary: CollectionDownload,
) -> bool {
    let fields = DownloadFields::of(summary);
    let changed = row.downloaded != fields.downloaded
        || row.partial != fields.partial
        || row.elsewhere != fields.elsewhere
        || row.dl_done != fields.done
        || row.dl_total != fields.total;
    if changed {
        row.downloaded = fields.downloaded;
        row.partial = fields.partial;
        row.elsewhere = fields.elsewhere;
        row.dl_done = fields.done;
        row.dl_total = fields.total;
    }
    changed
}

/// The download summary a card shows (to start the detail with).
fn row_download(row: &CollectionRow) -> CollectionDownload {
    let count = |n: i32| usize::try_from(n).unwrap_or(0);
    let songs = count(row.dl_total);
    let complete = count(row.dl_done);
    CollectionDownload {
        songs,
        complete,
        with_audio: if row.downloaded || row.partial || row.elsewhere {
            complete.max(1)
        } else {
            0
        },
        // Only "saved through other downloads" depends on the links.
        linked: if row.elsewhere { 0 } else { songs },
    }
}

/// Reads the download state of every loaded collection card and of the open
/// detail again, a moment after the last call (downloads finishing or being
/// removed, lyrics changing), and shows it.
pub(super) fn refresh_download_state(ctx: &Rc<AppContext>) {
    let weak = Rc::downgrade(ctx);
    ctx.library.download_state_timer.start(
        TimerMode::SingleShot,
        DOWNLOAD_STATE_DEBOUNCE,
        move || {
            if let Some(ctx) = weak.upgrade() {
                read_download_state(&ctx);
            }
        },
    );
}

fn read_download_state(ctx: &Rc<AppContext>) {
    let mut ids: Vec<String> = Vec::new();
    ctx.library.collections.update_where(|row| {
        if !row.placeholder {
            ids.push(row.id.to_string());
        }
        false
    });
    if let Some(open) = ctx.library.open.borrow().as_ref()
        && !ids.contains(&open.id)
    {
        ids.push(open.id.clone());
    }
    if ids.is_empty() {
        return;
    }
    let filter = ctx.library.source.get().filter();
    let format = ctx.config().download_format;
    ctx.with_backend(
        move |backend| async move {
            backend
                .storage
                .collection_downloads(filter, ids, format)
                .await
        },
        move |ctx, result| {
            let Some(Ok(summaries)) = result else { return };
            if ctx.library.source.get().filter() != filter {
                return; // the view changed meanwhile; its reload read it
            }
            ctx.library.collections.update_where(|row| {
                summaries
                    .get(row.id.as_str())
                    .is_some_and(|summary| apply_collection_download(row, *summary))
            });
            let open_summary = ctx
                .library
                .open
                .borrow()
                .as_ref()
                .and_then(|open| summaries.get(&open.id).copied());
            if let Some(summary) = open_summary {
                if let Some(open) = ctx.library.open.borrow_mut().as_mut() {
                    open.download = summary;
                }
                update_detail_download(ctx, &super::downloads::collection_progress(ctx));
            }
        },
    );
}

/// Updates the open collection detail's aggregate download indicator.
fn update_detail_download(ctx: &Rc<AppContext>, collections: &HashMap<String, (String, f32)>) {
    let Some((id, summary)) = ctx
        .library
        .open
        .borrow()
        .as_ref()
        .map(|open| (open.id.clone(), open.download))
    else {
        return;
    };
    ctx.with_ui(|ui| {
        let state = ui.global::<LibraryState>();
        let (status, progress) = collections
            .get(&id)
            .map_or((String::new(), 0.0), |(s, p)| (s.clone(), *p));
        let pinned = ctx.library.pinned.borrow().contains(&id);
        // The cards' definition: the members' files under the download
        // format (see `refresh_download_state`), and its own pins only to
        // tell "saved through other downloads"; never queue rows.
        let fields = DownloadFields::of(summary);
        state.set_detail_pinned(pinned);
        state.set_detail_keep(ctx.library.kept.borrow().contains(&id));
        state.set_detail_dl_status(status.as_str().into());
        state.set_detail_dl_progress(progress);
        state.set_detail_downloaded(fields.downloaded);
        state.set_detail_partial(fields.partial);
        state.set_detail_elsewhere(fields.elsewhere);
        state.set_detail_dl_done(fields.done);
        state.set_detail_dl_total(fields.total);
    });
}

fn set_loading(ctx: &AppContext, loading: bool) {
    ctx.with_ui(|ui| ui.global::<LibraryState>().set_loading(loading));
}

/// Shows an informational toast (hides by itself).
fn show_message(ctx: &AppContext, message: &str) {
    post_message(ctx, message, false);
}

/// Shows an error toast (stays until closed).
fn show_error(ctx: &AppContext, message: &str) {
    post_message(ctx, message, true);
}

fn post_message(ctx: &AppContext, message: &str, is_error: bool) {
    let seq = ctx.library.message_seq.get().wrapping_add(1);
    ctx.library.message_seq.set(seq);
    ctx.with_ui(|ui| {
        let state = ui.global::<LibraryState>();
        state.set_message(message.into());
        state.set_message_is_error(is_error);
        state.set_message_seq(seq);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(titles: &[&str]) -> NamedSongs {
        NamedSongs {
            count: titles.len(),
            titles: titles.iter().map(|&title| title.to_owned()).collect(),
        }
    }

    fn counted(count: usize) -> NamedSongs {
        NamedSongs {
            count,
            titles: Vec::new(),
        }
    }

    #[test]
    fn remove_collection_message_should_count_more_than_five_songs() {
        assert_eq!(
            remove_collection_message("Mix", &counted(37), &counted(3)),
            "Remove the downloads of “Mix”? 37 songs are deleted from this computer. \
             3 songs are kept because another album, playlist or single download still uses \
             them. Lyrics and song details stay."
        );
    }

    #[test]
    fn remove_collection_message_should_name_up_to_five_songs() {
        assert_eq!(
            remove_collection_message("Mix", &named(&["One", "Two"]), &named(&["Three"])),
            "Remove the downloads of “Mix”? “One” and “Two” are deleted from this computer. \
             “Three” is kept because another album, playlist or single download still uses it. \
             Lyrics and song details stay."
        );
    }

    #[test]
    fn remove_collection_message_should_say_when_nothing_is_deleted() {
        assert!(
            remove_collection_message("Mix", &counted(0), &counted(2))
                .contains("No downloaded file is deleted")
        );
    }

    #[test]
    fn removed_collection_message_should_count_more_than_five_songs() {
        assert_eq!(
            removed_collection_message(&counted(37), &counted(3)),
            "Removed the downloads of 37 songs. 3 songs are kept because other downloads still \
             need them."
        );
    }

    #[test]
    fn removed_collection_message_should_name_up_to_five_songs() {
        assert_eq!(
            removed_collection_message(&named(&["One"]), &counted(0)),
            "Removed the downloads of “One”."
        );
    }

    #[test]
    fn redownload_message_should_count_songs() {
        assert_eq!(
            [redownload_message(1), redownload_message(12)],
            [
                "Re-download 1 song? Existing files are replaced when the new ones finish."
                    .to_owned(),
                "Re-download 12 songs? Existing files are replaced when the new ones finish."
                    .to_owned(),
            ]
        );
    }

    fn track(id: &str) -> LibraryTrack {
        LibraryTrack {
            track: Track {
                track_number: Some(7),
                catalog_id: Some("900".to_owned()),
                artwork_url: Some(
                    "https://is1-ssl.mzstatic.com/image/thumb/M/a.jpg/{w}x{h}bb.jpg".to_owned(),
                ),
                ..Track::minimal(id, "Title", "Artist")
            },
            has_mapping: true,
            spatial_known: None,
        }
    }

    #[test]
    fn track_slot_should_carry_track_number() {
        let slot = track_slot(track("i.1"), &mut ImageCache::default(), &HashMap::new());
        assert_eq!(slot.row.track_number, 7);
    }

    #[test]
    fn track_slot_should_request_row_bucket() {
        let slot = track_slot(track("i.1"), &mut ImageCache::default(), &HashMap::new());
        assert_eq!(slot.art.map(|a| a.key.bucket), Some(Bucket::Row));
    }

    #[test]
    fn track_slot_should_apply_download_state_by_catalog_id() {
        let downloads = HashMap::from([("900".to_owned(), ("downloading".to_owned(), 0.5))]);
        let slot = track_slot(track("i.1"), &mut ImageCache::default(), &downloads);
        assert_eq!(slot.row.dl_status.as_str(), "downloading");
    }

    fn album(artist: Option<&str>, custom: bool) -> Playlist {
        Playlist {
            artist: artist.map(str::to_owned),
            is_custom: custom,
            ..Playlist::minimal("l.1", CollectionKind::Album, "Discovery")
        }
    }

    #[test]
    fn detail_query_should_ignore_search_and_filters() {
        let query = detail_query(LibraryFilter::AppleMusic, "p.1");
        assert_eq!(
            (
                query.search.as_str(),
                query.tracks,
                query.collection.as_deref()
            ),
            ("", TrackFilter::All, Some("p.1"))
        );
    }

    #[test]
    fn remove_collection_lyrics_message_should_name_the_collection_and_keep_imports() {
        let text = remove_collection_lyrics_message("Mix");
        assert!(text.contains("“Mix”") && text.contains("you imported are kept"));
    }

    fn mix_card() -> CollectionRow {
        collection_row(
            Playlist::minimal("p.1", CollectionKind::Playlist, "Mix"),
            None,
        )
    }

    #[test]
    fn finished_queue_rows_should_not_make_a_card_downloaded() {
        // The owner's "Mix 2026": the queue still holds "done" rows (and the
        // pins remain) after the downloads were cancelled or removed.
        let mut row = mix_card();
        row.pinned = true;
        let progress = HashMap::from([("p.1".to_owned(), ("done".to_owned(), 1.0))]);
        set_collection_dl(&mut row, &progress);
        apply_collection_download(
            &mut row,
            CollectionDownload {
                songs: 206,
                complete: 0,
                with_audio: 0,
                linked: 0,
            },
        );
        assert_eq!((row.downloaded, row.partial), (false, false));
    }

    #[test]
    fn card_should_show_how_much_of_a_partial_collection_is_downloaded() {
        let mut row = mix_card();
        apply_collection_download(
            &mut row,
            CollectionDownload {
                songs: 206,
                complete: 3,
                with_audio: 3,
                linked: 0,
            },
        );
        assert_eq!(
            (row.downloaded, row.partial, row.dl_done, row.dl_total),
            (false, true, 3, 206)
        );
    }

    #[test]
    fn card_should_be_downloaded_when_every_song_is_complete() {
        let mut row = mix_card();
        let changed = apply_collection_download(
            &mut row,
            CollectionDownload {
                songs: 2,
                complete: 2,
                with_audio: 2,
                linked: 2,
            },
        );
        assert_eq!((changed, row.downloaded, row.partial), (true, true, false));
    }

    #[test]
    fn card_should_be_saved_elsewhere_when_others_pin_its_complete_songs() {
        let mut row = mix_card();
        apply_collection_download(
            &mut row,
            CollectionDownload {
                songs: 3,
                complete: 3,
                with_audio: 3,
                linked: 1,
            },
        );
        assert_eq!(
            (row.downloaded, row.partial, row.elsewhere),
            (false, false, true)
        );
    }

    #[test]
    fn detail_should_start_from_a_saved_elsewhere_card() {
        let mut row = mix_card();
        apply_collection_download(
            &mut row,
            CollectionDownload {
                songs: 3,
                complete: 3,
                with_audio: 3,
                linked: 0,
            },
        );
        assert_eq!(
            row_download(&row).state(),
            CollectionDownloadState::SavedElsewhere
        );
    }

    fn on_disk(id: &str, catalog: &str) -> Track {
        Track {
            catalog_id: Some(catalog.to_owned()),
            audio: cider_offline_core::storage::TrackAudio::normal(format!("media/{catalog}.m4a")),
            ..Track::minimal(id, "Title", "Artist")
        }
    }

    fn not_on_disk(id: &str, catalog: &str) -> Track {
        Track {
            catalog_id: Some(catalog.to_owned()),
            ..Track::minimal(id, "Title", "Artist")
        }
    }

    #[test]
    fn collection_plan_should_link_songs_already_on_disk() {
        let tracks = [on_disk("i.1", "1"), not_on_disk("i.2", "2")];
        let plan = CollectionPlan::of(&tracks, DownloadFormat::Stereo, &HashMap::new());
        assert_eq!(plan.keys, ["1", "2"]);
    }

    #[test]
    fn collection_plan_should_queue_only_songs_missing_a_mix() {
        let tracks = [on_disk("i.1", "1"), not_on_disk("i.2", "2")];
        let plan = CollectionPlan::of(&tracks, DownloadFormat::Stereo, &HashMap::new());
        let missing: Vec<&str> = plan.missing.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(missing, ["i.2"]);
    }

    #[test]
    fn collection_plan_should_queue_nothing_when_every_file_exists() {
        let tracks = [on_disk("i.1", "1"), on_disk("i.2", "2")];
        let plan = CollectionPlan::of(&tracks, DownloadFormat::Stereo, &HashMap::new());
        assert_eq!(plan.missing.len(), 0);
    }

    #[test]
    fn link_message_should_count_the_linked_songs() {
        assert_eq!(
            download_collection_message(0, 11, "album"),
            "Linked 11 songs to this album. Nothing had to be downloaded."
        );
    }

    #[test]
    fn link_message_should_name_one_song_in_the_singular() {
        assert_eq!(
            download_collection_message(0, 1, "playlist"),
            "Linked 1 song to this playlist. Nothing had to be downloaded."
        );
    }

    #[test]
    fn download_message_should_count_the_queued_songs() {
        assert_eq!(
            download_collection_message(3, 8, "album"),
            "Added 3 songs to the download queue."
        );
    }

    #[test]
    fn download_message_should_say_when_nothing_changed() {
        assert_eq!(
            download_collection_message(0, 0, "playlist"),
            "Every song in this playlist is already downloaded."
        );
    }

    #[test]
    fn unchanged_summary_should_not_notify_the_card() {
        let mut row = mix_card();
        let summary = CollectionDownload {
            songs: 2,
            complete: 1,
            with_audio: 1,
            linked: 2,
        };
        apply_collection_download(&mut row, summary);
        assert!(!apply_collection_download(&mut row, summary));
    }

    #[test]
    fn detail_should_start_from_the_card_state() {
        let mut row = mix_card();
        let summary = CollectionDownload {
            songs: 5,
            complete: 2,
            with_audio: 2,
            linked: 5,
        };
        apply_collection_download(&mut row, summary);
        assert_eq!(row_download(&row).state(), summary.state());
    }

    #[test]
    fn summarize_members_should_count_a_song_listed_twice_once() {
        let mut a = track("i.1");
        a.track.audio = cider_offline_core::storage::TrackAudio::normal("a.m4a");
        let info = summarize_members(&[a.clone(), a, track("i.2")], DownloadFormat::Stereo);
        assert_eq!(
            (info.songs, info.download.songs, info.download.complete),
            (3, 2, 1)
        );
    }

    #[test]
    fn summarize_members_should_sum_known_durations() {
        let mut a = track("i.1");
        a.track.duration_ms = Some(60_000);
        let b = track("i.2");
        assert_eq!(
            summarize_members(&[a, b], DownloadFormat::Stereo).total_ms,
            60_000
        );
    }

    #[test]
    fn summarize_members_should_be_complete_only_with_all_audio() {
        let mut a = track("i.1");
        a.track.audio = cider_offline_core::storage::TrackAudio::normal("a.m4a");
        let b = track("i.2");
        assert_eq!(
            summarize_members(&[a, b], DownloadFormat::Stereo)
                .download
                .state(),
            CollectionDownloadState::Partial
        );
    }

    fn stereo_only(id: &str, spatial_known: Option<bool>) -> LibraryTrack {
        let mut track = track(id);
        track.track.audio = cider_offline_core::storage::TrackAudio::normal("a.m4a");
        track.spatial_known = spatial_known;
        track
    }

    #[test]
    fn stereo_only_atmos_song_should_be_incomplete_for_stereo_and_spatial() {
        let info = summarize_members(&[stereo_only("i.1", Some(true))], DownloadFormat::Both);
        assert_eq!(info.download.complete, 0);
    }

    #[test]
    fn stereo_only_song_without_spatial_should_be_complete_for_stereo_and_spatial() {
        let info = summarize_members(&[stereo_only("i.1", Some(false))], DownloadFormat::Both);
        assert_eq!(info.download.state(), CollectionDownloadState::Downloaded);
    }

    #[test]
    fn stereo_only_atmos_song_should_be_incomplete_for_spatial_where_available() {
        let info = summarize_members(
            &[stereo_only("i.1", Some(true))],
            DownloadFormat::SpatialWhereAvailable,
        );
        assert_eq!(info.download.complete, 0);
    }

    #[test]
    fn stereo_only_song_should_be_complete_for_stereo_only() {
        let info = summarize_members(&[stereo_only("i.1", None)], DownloadFormat::Stereo);
        assert_eq!(info.download.state(), CollectionDownloadState::Downloaded);
    }

    #[test]
    fn track_row_should_mark_a_spatial_mix() {
        let mut a = track("i.1");
        a.track
            .audio
            .set(AudioVariant::Spatial, Some("a.spatial.m4a".into()));
        let row = track_row(a.track, None, false);
        assert_eq!((row.has_audio, row.has_spatial), (true, true));
    }

    #[test]
    fn track_row_should_not_mark_a_stereo_only_song_spatial() {
        let mut a = track("i.1");
        a.track.audio = cider_offline_core::storage::TrackAudio::normal("a.m4a");
        assert!(!track_row(a.track, None, false).has_spatial);
    }

    #[test]
    fn wide_cards_should_use_the_large_bucket() {
        assert_eq!(
            (grid_bucket_for(182.0), grid_bucket_for(203.0)),
            (Bucket::Grid, Bucket::Hero)
        );
    }

    #[test]
    fn album_card_subtitle_should_be_the_artist() {
        let row = collection_row(album(Some("Daft Punk"), false), None);
        assert_eq!(row.subtitle.as_str(), "Daft Punk");
    }

    #[test]
    fn custom_card_subtitle_should_say_custom() {
        let row = collection_row(album(None, true), None);
        assert_eq!(row.subtitle.as_str(), "Album, Custom");
    }

    #[test]
    fn album_detail_subtitle_should_name_the_artist() {
        let row = collection_row(album(Some("Daft Punk"), false), None);
        assert_eq!(detail_subtitle(&row), "Album by Daft Punk");
    }

    #[test]
    fn songs_of_one_album_should_share_one_cover_key() {
        let a = track_slot(track("i.1"), &mut ImageCache::default(), &HashMap::new());
        let b = track_slot(track("i.2"), &mut ImageCache::default(), &HashMap::new());
        assert_eq!(a.art.map(|r| r.key), b.art.map(|r| r.key));
    }
}
