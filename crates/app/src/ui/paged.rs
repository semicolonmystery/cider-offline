//! A lazily paged `slint::Model` for long library lists.
//!
//! `row_count()` is the **total** number of rows the query matches (from a
//! `COUNT(*)`), so a `ListView` has a correct scrollbar from the first frame.
//! `row_data(i)` returns the loaded row, or a `placeholder` row while it asks
//! the owner (through [`Hooks::fetch`]) for page `i / page_size`. A `ListView`
//! only asks for the rows it shows, so this *is* load-on-scroll without any
//! scroll-position maths. When a page arrives the owner calls
//! [`PagedModel::apply_page`], which notifies `row_changed` for its rows.
//!
//! Covers work the same way: a loaded row without its image asks for it
//! through [`Hooks::want_art`]; [`PagedModel::apply_images`] fills every
//! loaded row sharing that cover and notifies only those rows. To bound memory,
//! at most `max_images` rows keep an image; the least recently shown ones lose
//! theirs silently (no notification, so on-screen rows of a non-virtualized
//! container never thrash) and ask again when they are shown next.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use slint::{Image, Model, ModelNotify, ModelTracker};

use super::artwork::{ArtKey, ArtRequest};
use super::{CollectionRow, TrackRow};

/// Row types the paged model can hold.
pub trait PagedRow: Clone + Default + 'static {
    /// A skeleton row for a not-yet-loaded index.
    fn placeholder() -> Self;
    /// Sets (or with `None` clears) the row's cover.
    fn set_artwork(&mut self, image: Option<Image>);
    /// Whether the row shows a cover.
    fn has_artwork(&self) -> bool;
}

impl PagedRow for TrackRow {
    fn placeholder() -> Self {
        Self {
            placeholder: true,
            ..Self::default()
        }
    }

    fn set_artwork(&mut self, image: Option<Image>) {
        self.has_artwork = image.is_some();
        self.artwork = image.unwrap_or_default();
    }

    fn has_artwork(&self) -> bool {
        self.has_artwork
    }
}

impl PagedRow for CollectionRow {
    fn placeholder() -> Self {
        Self {
            placeholder: true,
            ..Self::default()
        }
    }

    fn set_artwork(&mut self, image: Option<Image>) {
        self.has_artwork = image.is_some();
        self.artwork = image.unwrap_or_default();
    }

    fn has_artwork(&self) -> bool {
        self.has_artwork
    }
}

/// Callbacks into the owner. Both are invoked outside any model borrow and
/// must not call back into the model synchronously (they only schedule work).
pub struct Hooks {
    /// Load page `page` for query generation `generation`.
    pub fetch: Box<dyn Fn(u64, usize)>,
    /// Resolve these covers.
    pub want_art: Box<dyn Fn(Vec<ArtRequest>)>,
}

/// One loaded row and the cover it shows.
pub struct Slot<T> {
    /// The row as the UI sees it.
    pub row: T,
    /// The cover for this row, if it has one.
    pub art: Option<ArtRequest>,
}

struct LoadedSlot<T> {
    row: T,
    art: Option<ArtRequest>,
    /// Last `row_data` access (for image eviction).
    seen: u64,
}

struct State<T> {
    total: usize,
    generation: u64,
    /// A new query is in flight: don't fetch pages of the old one.
    resetting: bool,
    pages: HashMap<usize, Vec<LoadedSlot<T>>>,
    pending: HashSet<usize>,
    /// Art keys asked for and not answered yet (per model).
    asked: HashSet<ArtKey>,
    tick: u64,
    with_image: usize,
}

/// Paged, lazily loaded model; see the module docs.
pub struct PagedModel<T> {
    page_size: usize,
    max_images: usize,
    state: RefCell<State<T>>,
    hooks: RefCell<Option<Hooks>>,
    notify: ModelNotify,
    /// Rows notified by `row_changed` since start (for the perf harness).
    changed: Cell<usize>,
}

/// Work collected under the state borrow and run after releasing it.
#[derive(Default)]
struct Deferred {
    fetch: Vec<(u64, usize)>,
    art: Vec<ArtRequest>,
}

impl<T: PagedRow> PagedModel<T> {
    /// An empty model loading `page_size` rows per page and keeping at most
    /// `max_images` rows with a decoded cover.
    pub fn new(page_size: usize, max_images: usize) -> Self {
        Self {
            page_size: page_size.max(1),
            max_images,
            state: RefCell::new(State {
                total: 0,
                generation: 0,
                resetting: false,
                pages: HashMap::new(),
                pending: HashSet::new(),
                asked: HashSet::new(),
                tick: 0,
                with_image: 0,
            }),
            hooks: RefCell::new(None),
            notify: ModelNotify::default(),
            changed: Cell::new(0),
        }
    }

    /// Rows per page.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Installs the owner callbacks.
    pub fn set_hooks(&self, hooks: Hooks) {
        *self.hooks.borrow_mut() = Some(hooks);
    }

    /// Starts a new query: returns its generation. The current rows stay
    /// visible (no flash) until [`Self::reset`] delivers the first page, and
    /// results of older generations are ignored from now on.
    pub fn begin(&self) -> u64 {
        let mut state = self.state.borrow_mut();
        state.generation += 1;
        state.resetting = true;
        state.pending.clear();
        state.generation
    }

    /// Replaces the content with the first page of generation `generation`
    /// (ignored if a newer query started) and notifies a model reset.
    pub fn reset(&self, generation: u64, total: usize, first: Vec<Slot<T>>) {
        {
            let mut state = self.state.borrow_mut();
            if state.generation != generation {
                return;
            }
            state.total = total;
            state.resetting = false;
            state.pages.clear();
            state.pending.clear();
            state.asked.clear();
            state.with_image = 0;
            if !first.is_empty() || total == 0 {
                let slots = Self::loaded(&mut state, first);
                state.pages.insert(0, slots);
            }
        }
        self.notify.reset();
    }

    /// Stores page `page` of generation `generation` and notifies its rows.
    /// A changed total (rows added / removed meanwhile) resets the model.
    pub fn apply_page(&self, generation: u64, page: usize, total: usize, rows: Vec<Slot<T>>) {
        let (first, count, reset) = {
            let mut state = self.state.borrow_mut();
            if state.generation != generation {
                return;
            }
            state.pending.remove(&page);
            let reset = state.total != total;
            if reset {
                state.total = total;
                state.pages.clear();
                state.with_image = 0;
            }
            let count = rows.len();
            let slots = Self::loaded(&mut state, rows);
            state.pages.insert(page, slots);
            (page * self.page_size, count, reset)
        };
        if reset {
            self.notify.reset();
        } else {
            for row in first..first + count {
                self.row_changed(row);
            }
        }
    }

    fn loaded(state: &mut State<T>, rows: Vec<Slot<T>>) -> Vec<LoadedSlot<T>> {
        rows.into_iter()
            .map(|slot| {
                if slot.row.has_artwork() {
                    state.with_image += 1;
                }
                LoadedSlot {
                    row: slot.row,
                    art: slot.art,
                    seen: 0,
                }
            })
            .collect()
    }

    fn row_changed(&self, row: usize) {
        self.changed.set(self.changed.get() + 1);
        self.notify.row_changed(row);
    }

    /// Applies `change` to every loaded row and notifies the rows it reports
    /// as changed (no full rebuild). Returns how many changed.
    pub fn update_where(&self, mut change: impl FnMut(&mut T) -> bool) -> usize {
        let mut rows = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            for (page, slots) in &mut state.pages {
                for (index, slot) in slots.iter_mut().enumerate() {
                    if change(&mut slot.row) {
                        rows.push(page * self.page_size + index);
                    }
                }
            }
        }
        for &row in &rows {
            self.row_changed(row);
        }
        rows.len()
    }

    /// The first loaded row matching `predicate`.
    pub fn find(&self, mut predicate: impl FnMut(&T) -> bool) -> Option<T> {
        let state = self.state.borrow();
        state
            .pages
            .values()
            .flatten()
            .find(|slot| predicate(&slot.row))
            .map(|slot| slot.row.clone())
    }

    /// The cover request of the first loaded row matching `predicate`.
    pub fn art_of(&self, mut predicate: impl FnMut(&T) -> bool) -> Option<ArtRequest> {
        let state = self.state.borrow();
        state
            .pages
            .values()
            .flatten()
            .find(|slot| predicate(&slot.row))
            .and_then(|slot| slot.art.clone())
    }

    /// Puts arrived covers on every loaded row that shows them, notifying
    /// only those rows, then evicts images beyond `max_images`.
    pub fn apply_images(&self, images: &HashMap<ArtKey, Image>) {
        let mut rows = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            for key in images.keys() {
                state.asked.remove(key);
            }
            let mut added = 0;
            for (page, slots) in &mut state.pages {
                for (index, slot) in slots.iter_mut().enumerate() {
                    if slot.row.has_artwork() {
                        continue;
                    }
                    if let Some(image) = slot.art.as_ref().and_then(|art| images.get(&art.key)) {
                        slot.row.set_artwork(Some(image.clone()));
                        added += 1;
                        rows.push(page * self.page_size + index);
                    }
                }
            }
            state.with_image += added;
            self.evict(&mut state);
        }
        for row in rows {
            self.row_changed(row);
        }
    }

    /// Forgets that `keys` were asked for (their load failed or was lost), so
    /// rows may ask again later.
    pub fn forget_asked(&self, keys: &[ArtKey]) {
        let mut state = self.state.borrow_mut();
        for key in keys {
            state.asked.remove(key);
        }
    }

    /// Drops images of the least recently shown rows beyond `max_images`.
    /// Silent on purpose: see the module docs.
    fn evict(&self, state: &mut State<T>) {
        if state.with_image <= self.max_images {
            return;
        }
        let excess = state.with_image - self.max_images;
        let mut candidates: Vec<(u64, usize, usize)> = state
            .pages
            .iter()
            .flat_map(|(page, slots)| {
                slots
                    .iter()
                    .enumerate()
                    .filter(|(_, slot)| slot.row.has_artwork())
                    .map(move |(index, slot)| (slot.seen, *page, index))
            })
            .collect();
        candidates.sort_unstable();
        for (_, page, index) in candidates.into_iter().take(excess) {
            if let Some(slot) = state.pages.get_mut(&page).and_then(|s| s.get_mut(index)) {
                slot.row.set_artwork(None);
                state.with_image -= 1;
            }
        }
    }

    /// (loaded rows, rows holding an image, rows notified so far).
    pub fn stats(&self) -> (usize, usize, usize) {
        let state = self.state.borrow();
        let loaded = state.pages.values().map(Vec::len).sum();
        (loaded, state.with_image, self.changed.get())
    }

    fn run(&self, deferred: Deferred) {
        let hooks = self.hooks.borrow();
        let Some(hooks) = hooks.as_ref() else {
            return;
        };
        for (generation, page) in deferred.fetch {
            (hooks.fetch)(generation, page);
        }
        if !deferred.art.is_empty() {
            (hooks.want_art)(deferred.art);
        }
    }

    /// Queues the fetch of `page` unless it is loaded, pending or past the end.
    fn want_page(&self, state: &mut State<T>, page: usize, deferred: &mut Deferred) {
        if state.resetting
            || page * self.page_size >= state.total
            || state.pages.contains_key(&page)
            || !state.pending.insert(page)
        {
            return;
        }
        deferred.fetch.push((state.generation, page));
    }
}

impl<T: PagedRow> Model for PagedModel<T> {
    type Data = T;

    fn row_count(&self) -> usize {
        self.state.borrow().total
    }

    fn row_data(&self, row: usize) -> Option<T> {
        let mut deferred = Deferred::default();
        let data = {
            let mut state = self.state.borrow_mut();
            if row >= state.total {
                return None;
            }
            state.tick += 1;
            let tick = state.tick;
            let page = row / self.page_size;
            let index = row % self.page_size;
            // Load the next page a quarter page ahead of the end of this one.
            if index >= self.page_size * 3 / 4 {
                self.want_page(&mut state, page + 1, &mut deferred);
            }
            let loaded = {
                let State { pages, asked, .. } = &mut *state;
                pages
                    .get_mut(&page)
                    .and_then(|slots| slots.get_mut(index))
                    .map(|slot| {
                        slot.seen = tick;
                        if !slot.row.has_artwork()
                            && let Some(art) = &slot.art
                            && asked.insert(art.key.clone())
                        {
                            deferred.art.push(art.clone());
                        }
                        slot.row.clone()
                    })
            };
            if let Some(row) = loaded {
                row
            } else {
                self.want_page(&mut state, page, &mut deferred);
                T::placeholder()
            }
        };
        self.run(deferred);
        Some(data)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn row(id: &str) -> Slot<TrackRow> {
        Slot {
            row: TrackRow {
                id: id.into(),
                ..TrackRow::default()
            },
            art: None,
        }
    }

    fn page(start: usize, len: usize) -> Vec<Slot<TrackRow>> {
        (start..start + len).map(|n| row(&n.to_string())).collect()
    }

    fn model_with_log() -> (PagedModel<TrackRow>, Rc<RefCell<Vec<usize>>>) {
        let model = PagedModel::new(10, 100);
        let log = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&log);
        model.set_hooks(Hooks {
            fetch: Box::new(move |_, page| sink.borrow_mut().push(page)),
            want_art: Box::new(|_| {}),
        });
        (model, log)
    }

    #[test]
    fn row_count_should_be_query_total() {
        let (model, _) = model_with_log();
        let generation = model.begin();
        model.reset(generation, 35, page(0, 10));
        assert_eq!(model.row_count(), 35);
    }

    #[test]
    fn unloaded_row_should_be_placeholder_and_fetch_its_page() {
        let (model, log) = model_with_log();
        let generation = model.begin();
        model.reset(generation, 35, page(0, 10));
        let data = model.row_data(25).unwrap();
        assert!(data.placeholder && log.borrow().as_slice() == [2]);
    }

    #[test]
    fn page_should_be_fetched_once() {
        let (model, log) = model_with_log();
        let generation = model.begin();
        model.reset(generation, 35, page(0, 10));
        model.row_data(21);
        model.row_data(22);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn applied_page_should_serve_rows() {
        let (model, _) = model_with_log();
        let generation = model.begin();
        model.reset(generation, 35, page(0, 10));
        model.apply_page(generation, 2, 35, page(20, 10));
        assert_eq!(model.row_data(23).unwrap().id, "23");
    }

    #[test]
    fn stale_generation_page_should_be_ignored() {
        let (model, _) = model_with_log();
        let old = model.begin();
        model.reset(old, 35, page(0, 10));
        let new = model.begin();
        model.reset(new, 35, page(100, 10));
        model.apply_page(old, 2, 35, page(20, 10));
        assert!(model.row_data(23).unwrap().placeholder);
    }

    #[test]
    fn row_near_page_end_should_prefetch_next_page() {
        let (model, log) = model_with_log();
        let generation = model.begin();
        model.reset(generation, 35, page(0, 10));
        model.row_data(8);
        assert_eq!(log.borrow().as_slice(), [1]);
    }

    #[test]
    fn update_where_should_touch_only_matching_rows() {
        let (model, _) = model_with_log();
        let generation = model.begin();
        model.reset(generation, 10, page(0, 10));
        let changed = model.update_where(|r| {
            if r.id == "3" {
                r.busy = true;
                true
            } else {
                false
            }
        });
        assert_eq!(changed, 1);
    }
}
