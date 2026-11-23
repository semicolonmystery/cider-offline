//! Screenshot harness (`--screenshot`): renders one Library view with the
//! real window, renderer, paged models and cover pipeline against the app's
//! store, then saves a PNG. No proxy is started and nothing is written to the
//! store; the network is used only for covers missing from the disk cache.
//!
//! ```text
//! cider-offline --screenshot out.png [--width 900] [--height 700]
//!     [--page library|songs|detail|overview|settings] [--collection NAME]
//!     [--search TEXT] [--dl-state idle|downloading|paused|failed|done]
//!     [--hover X,Y] [--click X,Y] [--right-click X,Y] [--overlay add|drawer|confirm]
//!     [--toast TEXT [--toast-error]] [--expand] [--unlinked-auto]
//! ```
//!
//! `--overlay remove` shows the "Remove downloads" confirmation of
//! `--collection` (or the first collection) with its real counts;
//! `--overlay cancel-all` shows the "Cancel all" confirmation of the demo
//! drawer rows. The Settings page counts the store's unlinked downloads;
//! `--unlinked-auto` shows them set to "Automatically".
//!
//! `--page detail` opens `--collection`, or the first collection of the
//! Apple Music library when it is not given.
//!
//! `--expand` opens the Settings page's "More options" disclosures; the
//! Settings page shows the lyrics settings of `config.toml`.
//!
//! `--page overview` shows the Overview page with demo counters, with "Route
//! Cider through this proxy" on. The settings file shown (there and on the
//! Settings page) is `cider_config_path` of `config.toml`; Cider's real
//! settings file is never located or read. Overview options:
//!
//! ```text
//!     [--mode active|passthrough] [--issue certificate|cider|session|error]
//!     [--anim on:P|off:P] [--notes-time SECONDS]
//!     [--sequence on|off --frames N]
//! ```
//!
//! `--anim on:0.3` freezes the ON ripple at 30 % (`off:P` the OFF fade;
//! `--mode` is set to match). `--sequence` saves `N` frames of the whole
//! animation as `<out>-000.png`, `<out>-001.png`, … (a 30 fps timeline: the
//! notes drift by 1/30 s per frame) instead of one shot.
//!
//! `--hold SECONDS` keeps the window open that long before the shot (with
//! the Overview's notes drifting as in the app), to measure CPU and memory.
//!
//! `--section proxy|certificate|cider|startup|downloads|library|lyrics|storage`
//! opens the Settings page scrolled to that section, as the Overview's "Go
//! to Settings" link does.
//!
//! `--right-click` opens a native context menu, whose modal loop blocks
//! until the menu is dismissed; capture it from the screen instead.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use cider_offline_core::{Library, LibraryFilter, LibraryQuery, Storage};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle as _, LogicalPosition, ModelRc};

use super::artwork::{self, ArtRequest, Bucket, ImageCache};
use super::library::{
    COLLECTION_PAGE, NamedSongs, TRACK_PAGE, collection_slot, detail_subtitle, grid_bucket_for,
    summarize_members, track_slot,
};
use super::paged::{Hooks, PagedModel, Slot};
use super::{
    AppWindow, CiderState, CollectionRow, DialogState, DownloadRow, DownloadsState,
    LibraryAddState, LibraryState, LibraryUi, OverviewAnim, ProxyState, SettingsState, TrackRow,
    format,
};

/// The flag that selects this mode.
pub const FLAG: &str = "--screenshot";

/// Command-line options.
#[derive(Debug, Clone)]
pub struct Options {
    out: PathBuf,
    width: f32,
    height: f32,
    page: String,
    collection: Option<String>,
    search: String,
    dl_state: Option<String>,
    hover: Option<(f32, f32)>,
    right_click: Option<(f32, f32)>,
    click: Option<(f32, f32)>,
    overlay: String,
    toast: Option<String>,
    toast_error: bool,
    expand: bool,
    unlinked_auto: bool,
    /// Overview: "active" (default) or "passthrough".
    mode: String,
    issue: String,
    /// Overview: a frozen animation frame, `(on, progress)`.
    anim: Option<(bool, f32)>,
    notes_time: f32,
    /// Overview: save a frame sequence of the ON (`true`) or OFF animation.
    sequence: Option<bool>,
    frames: u32,
    section: String,
    /// Keep the window open (and the notes drifting) this many seconds
    /// before the shot, so its CPU and memory can be measured.
    hold: u32,
}

/// Returns the options when `--screenshot <out.png>` is present.
pub fn parse(args: &[String]) -> Option<Options> {
    let at = args.iter().position(|a| a == FLAG)?;
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let point = |name: &str| {
        value(name).and_then(|v| {
            let (x, y) = v.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        })
    };
    Some(Options {
        out: PathBuf::from(args.get(at + 1)?),
        width: value("--width")
            .and_then(|v| v.parse().ok())
            .unwrap_or(900.0),
        height: value("--height")
            .and_then(|v| v.parse().ok())
            .unwrap_or(700.0),
        page: value("--page").unwrap_or_else(|| "library".to_owned()),
        collection: value("--collection"),
        search: value("--search").unwrap_or_default(),
        dl_state: value("--dl-state"),
        hover: point("--hover"),
        right_click: point("--right-click"),
        click: point("--click"),
        overlay: value("--overlay").unwrap_or_default(),
        toast: value("--toast"),
        toast_error: args.iter().any(|a| a == "--toast-error"),
        expand: args.iter().any(|a| a == "--expand"),
        unlinked_auto: args.iter().any(|a| a == "--unlinked-auto"),
        mode: value("--mode").unwrap_or_default(),
        issue: value("--issue").unwrap_or_default(),
        anim: value("--anim").and_then(|v| {
            let (kind, progress) = v.split_once(':')?;
            Some((kind == "on", progress.trim().parse().ok()?))
        }),
        notes_time: value("--notes-time")
            .and_then(|v| v.parse().ok())
            .unwrap_or(12.0),
        sequence: value("--sequence").map(|kind| kind == "on"),
        frames: value("--frames").and_then(|v| v.parse().ok()).unwrap_or(48),
        section: value("--section").unwrap_or_default(),
        hold: value("--hold").and_then(|v| v.parse().ok()).unwrap_or(0),
    })
}

/// Everything loaded from the store before the window is shown.
struct Loaded {
    collections: Vec<cider_offline_core::LibraryCollection>,
    tracks: Vec<cider_offline_core::LibraryTrack>,
    total_tracks: usize,
    total_collections: usize,
    detail: Option<cider_offline_core::LibraryCollection>,
    /// Collections with a download pin, and those set to "Keep downloaded".
    pinned: Vec<String>,
    kept: Vec<String>,
    /// How much of each shown collection (and the detail) is downloaded.
    summaries: HashMap<String, cider_offline_core::CollectionDownload>,
}

/// Renders the view and writes the PNG.
pub fn run(options: &Options) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating the tokio runtime")?;
    let paths = crate::startup::discover_paths()?;
    let (storage, library) = rt.block_on(async {
        let storage = Storage::open_async(paths.database_file()).await?;
        let library = Library::new(storage.clone(), paths.clone())?;
        anyhow::Ok((storage, library))
    })?;
    let loaded = rt.block_on(load(&storage, options))?;

    let app = AppWindow::new().context("creating the window")?;
    app.window()
        .set_size(slint::LogicalSize::new(options.width, options.height));
    let ui = Ui::new(&app, &loaded, options);
    let config = cider_offline_core::Config::load_or_default(&paths.config_file())
        .context("reading config.toml")?;
    prepare(&app, options, &rt, &storage, &paths, &config, &loaded)?;
    let images = Rc::new(RefCell::new(ImageCache::default()));
    let wanted: Rc<RefCell<Vec<ArtRequest>>> = Rc::default();
    let tracks = paged_tracks(&loaded, &images, &wanted, options);
    let collections = paged_collections(&loaded, &images, &wanted, ui.grid_bucket, options);
    {
        let state = app.global::<LibraryState>();
        state.set_tracks(ModelRc::from(Rc::clone(&tracks)));
        state.set_collections(ModelRc::from(Rc::clone(&collections)));
    }
    show_detail(&app, &loaded, &rt, &library, &images, options);

    app.show().context("showing the window")?;
    let tick = slint::Timer::default();
    let rounds = Rc::new(std::cell::Cell::new(0u32));
    let weak = app.as_weak();
    let options = options.clone();
    let failure: Rc<RefCell<Option<anyhow::Error>>> = Rc::default();
    let failure_out = Rc::clone(&failure);
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(app) = weak.upgrade() else { return };
            let round = rounds.get() + 1;
            rounds.set(round);
            // Covers the visible rows asked for since the last tick.
            let requests = std::mem::take(&mut *wanted.borrow_mut());
            if !requests.is_empty() {
                let (hits, misses) = images.borrow_mut().triage(requests);
                tracks.apply_images(&hits);
                collections.apply_images(&hits);
                let decoded = rt.block_on(artwork::load(library.clone(), misses));
                let created = images.borrow_mut().accept(decoded);
                tracks.apply_images(&created);
                collections.apply_images(&created);
            }
            interact(&app, &options, round);
            if round >= 8 + options.hold * 4 {
                let saved = match options.sequence {
                    Some(on) => save_sequence(&app, &options, on),
                    None => save(&app, &options.out),
                };
                if let Err(error) = saved {
                    *failure_out.borrow_mut() = Some(error);
                }
                let _ = slint::quit_event_loop();
            }
        },
    );
    slint::run_event_loop().context("running the event loop")?;
    drop(tick);
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    Ok(())
}

/// The scripted input of one tick: pointer events and the toast.
fn interact(app: &AppWindow, options: &Options, round: u32) {
    match round {
        // Ignore wherever the real mouse pointer happens to be.
        1 => pointer(app, WindowEvent::PointerExited),
        2 => {
            if let Some(text) = &options.toast {
                let state = app.global::<LibraryState>();
                state.set_message(text.as_str().into());
                state.set_message_is_error(options.toast_error);
                state.set_message_seq(state.get_message_seq() + 1);
            }
        }
        // Again just before the snapshot, unless the shot hovers on purpose.
        7 if options.hover.is_none()
            && options.right_click.is_none()
            && options.click.is_none() =>
        {
            pointer(app, WindowEvent::PointerExited);
        }
        4 => {
            if let Some((x, y)) = options.hover {
                let position = LogicalPosition::new(x, y);
                pointer(app, WindowEvent::PointerMoved { position });
            }
            if let Some((x, y)) = options.click {
                let position = LogicalPosition::new(x, y);
                let button = PointerEventButton::Left;
                pointer(app, WindowEvent::PointerMoved { position });
                pointer(app, WindowEvent::PointerPressed { position, button });
                pointer(app, WindowEvent::PointerReleased { position, button });
            }
            if let Some((x, y)) = options.right_click {
                let position = LogicalPosition::new(x, y);
                let button = PointerEventButton::Right;
                pointer(app, WindowEvent::PointerMoved { position });
                pointer(app, WindowEvent::PointerPressed { position, button });
                pointer(app, WindowEvent::PointerReleased { position, button });
            }
        }
        _ => {}
    }
}

/// Page and overlay state that needs the store: the Settings page's
/// lyrics and unlinked downloads, the Overview demo, and the dialogs.
fn prepare(
    app: &AppWindow,
    options: &Options,
    rt: &tokio::runtime::Runtime,
    storage: &Storage,
    paths: &cider_offline_core::AppPaths,
    config: &cider_offline_core::Config,
    loaded: &Loaded,
) -> anyhow::Result<()> {
    match options.page.as_str() {
        "settings" => {
            let state = app.global::<SettingsState>();
            super::settings_page::fill_lyrics(&state, config);
            super::settings_page::show_folders(&state, paths);
            state.set_lyrics_more_open(options.expand);
            let (_, report) = rt.block_on(cider_offline_core::unlinked_downloads(
                storage,
                &paths.media_dir(),
            ))?;
            super::settings_page::set_unlinked(&state, &report);
            state.set_unlinked_mode(i32::from(options.unlinked_auto));
            state.set_unlinked_hours(i32::try_from(config.unlinked_cleanup_hours()).unwrap_or(6));
            demo_proxy(app, config);
        }
        "overview" => demo_overview(app, config, options),
        _ => {}
    }
    match options.overlay.as_str() {
        "remove" => {
            let collection = loaded.detail.as_ref().or(loaded.collections.first());
            if let Some(collection) = collection {
                let id = collection.collection.id.clone();
                let (deleted, kept) = rt.block_on(async {
                    let keys =
                        super::library::collection_keys(storage, LibraryFilter::AppleMusic, &id)
                            .await
                            .map_err(anyhow::Error::msg)?;
                    let preview = storage.download_removal_preview(&id, keys).await?;
                    anyhow::Ok((
                        NamedSongs::of(storage, &preview.deleted).await,
                        NamedSongs::of(storage, &preview.kept).await,
                    ))
                })?;
                show_dialog(
                    app,
                    "Remove downloads",
                    &super::library::remove_collection_message(
                        &collection.collection.name,
                        &deleted,
                        &kept,
                    ),
                    "Remove downloads",
                );
            }
        }
        "cancel-all" => {
            // The unfinished demo drawer rows (see `--overlay drawer`).
            let titles: Vec<String> = demo_rows(loaded)
                .into_iter()
                .filter(|row| row.status != "done")
                .map(|row| row.title.to_string())
                .collect();
            show_dialog(
                app,
                "Cancel all downloads",
                &super::downloads::cancel_message(&titles, true),
                "Cancel all",
            );
        }
        _ => {}
    }
    Ok(())
}

/// Opens the app's confirmation dialog with a destructive confirm button.
fn show_dialog(app: &AppWindow, title: &str, message: &str, confirm: &str) {
    let state = app.global::<DialogState>();
    state.set_title(title.into());
    state.set_message(message.into());
    state.set_confirm_label(confirm.into());
    state.set_destructive(true);
    state.set_open(true);
}

fn pointer(app: &AppWindow, event: WindowEvent) {
    if let Err(error) = app.window().dispatch_event_with_result(event) {
        tracing::warn!(%error, "could not dispatch a pointer event");
    }
}

/// Saves `options.frames` frames of the ON or OFF animation, numbered after
/// `options.out`.
fn save_sequence(app: &AppWindow, options: &Options, on: bool) -> anyhow::Result<()> {
    let anim = app.global::<OverviewAnim>();
    app.global::<ProxyState>().set_active(on);
    let frames = options.frames.max(2);
    let stem = options.out.with_extension("");
    for frame in 0..frames {
        #[expect(clippy::cast_precision_loss, reason = "frame counts are small")]
        let progress = frame as f32 / (frames - 1) as f32;
        if on {
            anim.set_freeze_on(progress);
        } else {
            anim.set_freeze_off(progress);
        }
        #[expect(clippy::cast_precision_loss, reason = "frame counts are small")]
        anim.set_time(options.notes_time + frame as f32 / 30.0);
        let path = PathBuf::from(format!("{}-{frame:03}.png", stem.display()));
        save(app, &path)?;
    }
    Ok(())
}

fn save(app: &AppWindow, out: &std::path::Path) -> anyhow::Result<()> {
    let pixels = app
        .window()
        .take_snapshot()
        .context("taking the snapshot")?;
    let image =
        image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
            .context("snapshot buffer size")?;
    image.save(out).context("writing the PNG")?;
    tracing::info!(path = %out.display(), "screenshot saved");
    Ok(())
}

async fn load(storage: &Storage, options: &Options) -> anyhow::Result<Loaded> {
    let filter = LibraryFilter::AppleMusic;
    let source = LibraryQuery {
        filter,
        ..LibraryQuery::default()
    };
    let search = LibraryQuery {
        search: options.search.clone(),
        ..source.clone()
    };
    let collections = storage
        .collections_page(search.clone(), 0, COLLECTION_PAGE)
        .await?
        .items;
    let total_tracks = storage.tracks_page(source.clone(), 0, 0).await?.total;
    let total_collections = storage.collections_page(source.clone(), 0, 0).await?.total;
    let mut detail = None;
    let tracks = if options.page == "detail" {
        let all = storage
            .collections_page(source.clone(), 0, usize::MAX)
            .await?;
        let found = match options.collection.as_deref() {
            Some(name) => all
                .items
                .into_iter()
                .find(|c| c.collection.name.trim() == name),
            None => all.items.into_iter().next(),
        };
        let Some(found) = found else {
            match options.collection.as_deref() {
                Some(name) => bail!("collection {name:?} not found"),
                None => bail!("the Apple Music library has no collections"),
            }
        };
        let query = LibraryQuery {
            filter,
            collection: Some(found.collection.id.clone()),
            ..LibraryQuery::default()
        };
        detail = Some(found);
        storage.tracks_page(query, 0, usize::MAX).await?.items
    } else {
        storage.tracks_page(search, 0, TRACK_PAGE).await?.items
    };
    let mut ids: Vec<String> = collections
        .iter()
        .chain(detail.as_ref())
        .map(|entry| entry.collection.id.clone())
        .collect();
    ids.dedup();
    let summaries = storage
        .collection_downloads(filter, ids, cider_offline_core::DownloadFormat::default())
        .await?;
    let pinned = storage.pinned_collections().await?;
    let kept = storage
        .download_collections()
        .await?
        .into_iter()
        .filter(|collection| collection.keep)
        .map(|collection| collection.owner)
        .collect();
    Ok(Loaded {
        collections,
        tracks,
        total_tracks,
        total_collections,
        detail,
        pinned,
        kept,
        summaries,
    })
}

fn art_hooks(wanted: &Rc<RefCell<Vec<ArtRequest>>>) -> Hooks {
    let sink = Rc::clone(wanted);
    Hooks {
        fetch: Box::new(|_, _| {}),
        want_art: Box::new(move |requests| sink.borrow_mut().extend(requests)),
    }
}

/// The demo download states put on the first rows (`--dl-state`).
const ROW_STATES: [(&str, f32); 5] = [
    ("queued", 0.0),
    ("downloading", 0.4),
    ("paused", 0.6),
    ("failed", 0.0),
    ("", 0.0),
];

fn paged_tracks(
    loaded: &Loaded,
    images: &Rc<RefCell<ImageCache>>,
    wanted: &Rc<RefCell<Vec<ArtRequest>>>,
    options: &Options,
) -> Rc<PagedModel<TrackRow>> {
    let mut slots: Vec<Slot<TrackRow>> = {
        let mut images = images.borrow_mut();
        loaded
            .tracks
            .iter()
            .cloned()
            .map(|item| track_slot(item, &mut images, &HashMap::new()))
            .collect()
    };
    if options.dl_state.is_some() {
        for (slot, (status, progress)) in slots.iter_mut().zip(ROW_STATES) {
            slot.row.dl_status = status.into();
            slot.row.dl_progress = progress;
            if status.is_empty() {
                slot.row.has_audio = true;
            }
        }
    }
    let model = Rc::new(PagedModel::new(TRACK_PAGE, 400));
    model.set_hooks(art_hooks(wanted));
    let total = slots.len();
    let generation = model.begin();
    let mut pages = Vec::new();
    while !slots.is_empty() {
        let rest = slots.split_off(slots.len().min(TRACK_PAGE));
        pages.push(std::mem::replace(&mut slots, rest));
    }
    let mut pages = pages.into_iter();
    model.reset(generation, total, pages.next().unwrap_or_default());
    for (index, page) in pages.enumerate() {
        model.apply_page(generation, index + 1, total, page);
    }
    model
}

fn paged_collections(
    loaded: &Loaded,
    images: &Rc<RefCell<ImageCache>>,
    wanted: &Rc<RefCell<Vec<ArtRequest>>>,
    bucket: Bucket,
    options: &Options,
) -> Rc<PagedModel<CollectionRow>> {
    let mut slots: Vec<Slot<CollectionRow>> = {
        let mut images = images.borrow_mut();
        loaded
            .collections
            .iter()
            .cloned()
            .map(|item| {
                let mut slot = collection_slot(
                    item,
                    bucket,
                    &mut images,
                    &HashMap::new(),
                    &loaded.summaries,
                );
                let id = slot.row.id.to_string();
                slot.row.pinned = loaded.pinned.contains(&id);
                slot.row.keep = loaded.kept.contains(&id);
                slot
            })
            .collect()
    };
    if options.dl_state.is_some() {
        for (slot, (status, progress)) in slots.iter_mut().zip(ROW_STATES) {
            slot.row.dl_status = status.into();
            slot.row.dl_progress = progress;
            slot.row.downloaded = status.is_empty();
        }
    }
    let model = Rc::new(PagedModel::new(COLLECTION_PAGE, 120));
    model.set_hooks(art_hooks(wanted));
    let total = slots.len();
    let generation = model.begin();
    model.reset(generation, total, slots);
    model
}

/// The window-level state of the shot.
struct Ui {
    grid_bucket: Bucket,
}

impl Ui {
    fn new(app: &AppWindow, loaded: &Loaded, options: &Options) -> Self {
        // Same geometry as library.slint: sidebar, page inset, grid columns.
        let sidebar = if options.width < 1000.0 { 64.0 } else { 220.0 };
        let page = options.width - sidebar;
        let pad = if page >= 1100.0 { 32.0 } else { 24.0 };
        let w = page - 2.0 * pad;
        let cols = ((w + 20.0) / 200.0).floor().max(2.0);
        let card = (w - 20.0 * (cols - 1.0)) / cols;
        let page_index = match options.page.as_str() {
            "overview" => 0,
            "settings" => 2,
            _ => 1,
        };
        app.global::<SettingsState>()
            .set_focus_section(options.section.as_str().into());
        app.set_page(page_index);
        let proxy = app.global::<ProxyState>();
        proxy.set_running(true);
        proxy.set_starting(false);
        proxy.set_trust_state("trusted".into());
        let state = app.global::<LibraryState>();
        let collections = loaded.total_collections;
        state.set_total_collections(i32::try_from(collections).unwrap_or(i32::MAX));
        state.set_total_tracks(i32::try_from(loaded.total_tracks).unwrap_or(i32::MAX));
        state.set_count_line(format::library_counts(collections, loaded.total_tracks).into());
        state.set_view(i32::from(options.page == "songs"));
        state.set_search(options.search.as_str().into());
        match options.overlay.as_str() {
            "add" => app.global::<LibraryAddState>().set_open(true),
            "drawer" => {
                demo_downloads(app, loaded);
                app.global::<LibraryUi>().set_drawer_open(true);
            }
            "confirm" => {
                let ui = app.global::<LibraryUi>();
                ui.set_confirm_title(
                    loaded
                        .tracks
                        .first()
                        .map_or("Song", |t| t.track.title.as_str())
                        .into(),
                );
                ui.set_confirm_id("shot".into());
            }
            _ => {}
        }
        Self {
            grid_bucket: grid_bucket_for(card),
        }
    }
}

/// The Overview shot (`--page overview`): demo counters and Cider row, the
/// mode, an optional issue and a frozen animation frame.
fn demo_overview(app: &AppWindow, config: &cider_offline_core::Config, options: &Options) {
    demo_proxy(app, config);
    let proxy = app.global::<ProxyState>();
    let cider = app.global::<CiderState>();
    match options.issue.as_str() {
        "certificate" => {
            proxy.set_trust_state("untrusted".into());
            proxy.set_trust_message(
                "Cider will reject the proxy's HTTPS traffic until the certificate is trusted."
                    .into(),
            );
        }
        "cider" => cider.set_routed(false),
        "session" => proxy.set_session_captured(false),
        "error" => proxy.set_last_error(
            "Could not start the proxy: address 127.0.0.1:8000 is already in use".into(),
        ),
        _ => {}
    }
    let anim = app.global::<OverviewAnim>();
    anim.set_live(true);
    anim.set_notes(ModelRc::new(slint::VecModel::from(
        super::overview::note_seeds(),
    )));
    anim.set_time(options.notes_time);
    let mut active = options.mode != "passthrough";
    match options.anim {
        Some((true, progress)) => {
            active = true;
            anim.set_freeze_on(progress);
        }
        Some((false, progress)) => {
            active = false;
            anim.set_freeze_off(progress);
        }
        None => {}
    }
    proxy.set_active(active);
}

/// Demo proxy counters and Cider row, shared by the Overview and Settings
/// shots, with Cider routed through the proxy.
fn demo_proxy(app: &AppWindow, config: &cider_offline_core::Config) {
    let listen = config.listen_addr().to_string();
    let proxy = app.global::<ProxyState>();
    proxy.set_address(listen.as_str().into());
    proxy.set_requests_seen(format::count(1_596).into());
    proxy.set_served_locally(format::count(119).into());

    let cider = app.global::<CiderState>();
    cider.set_automatic(crate::cider::SUPPORT == crate::cider::Support::Automatic);
    cider.set_guide(crate::cider::manual_guide(&listen).into());
    cider.set_available(true);
    cider.set_routed(true);
    let shown = config
        .cider_config_path
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    cider.set_config_path(shown.into());
}

/// Mixed demo rows for the Downloads drawer shot (`--overlay drawer`), built
/// from the first loaded songs: downloading, queued with a retry note,
/// failed, can't download yet, paused and done.
fn demo_downloads(app: &AppWindow, loaded: &Loaded) {
    let rows = demo_rows(loaded);
    let summary = super::downloads::Summary::of(&rows);
    super::downloads::set_summary(app, summary);
    app.global::<DownloadsState>()
        .set_rows(ModelRc::new(slint::VecModel::from(rows)));
}

/// The demo drawer rows of [`demo_downloads`].
fn demo_rows(loaded: &Loaded) -> Vec<DownloadRow> {
    const STATES: [(&str, f32, &str, &str); 7] = [
        ("downloading", 0.42, "Downloading, 42%", ""),
        ("downloading", 0.09, "License, 9%", ""),
        ("queued", 0.0, "Retrying in 12 s (attempt 2 of 4)", ""),
        (
            "failed",
            0.0,
            "Failed: Widevine license error: empty license response",
            "empty license response",
        ),
        (
            "unavailable",
            0.0,
            "Can't download yet: Music video: downloading its audio is not supported yet",
            "Music video",
        ),
        ("paused", 0.6, "Paused at 60%", ""),
        ("done", 1.0, "Downloaded", ""),
    ];
    let mut rows: Vec<DownloadRow> = loaded
        .tracks
        .iter()
        .zip(STATES.iter().cycle())
        .take(10)
        .map(|(item, (status, progress, detail, message))| DownloadRow {
            id: item.track.id.as_str().into(),
            track_id: item.track.id.as_str().into(),
            title: item.track.title.as_str().into(),
            artist: item.track.artist.as_str().into(),
            album: item.track.album.clone().unwrap_or_default().into(),
            status: (*status).into(),
            progress: *progress,
            detail: (*detail).into(),
            message: (*message).into(),
            source: "playlist:demo".into(),
            source_name: "Demo playlist".into(),
            spatial: item
                .track
                .audio
                .get(cider_offline_core::storage::AudioVariant::Spatial)
                .is_some(),
            ..DownloadRow::default()
        })
        .collect();
    // A couple more finished rows so the batch looks like a real one.
    for row in rows.iter_mut().skip(7) {
        row.status = "done".into();
        row.progress = 1.0;
        row.detail = "Downloaded".into();
    }
    rows
}

/// Opens the detail view of `loaded.detail` (if any) with its hero cover.
fn show_detail(
    app: &AppWindow,
    loaded: &Loaded,
    rt: &tokio::runtime::Runtime,
    library: &Library,
    images: &Rc<RefCell<ImageCache>>,
    options: &Options,
) {
    let Some(detail) = &loaded.detail else { return };
    let state = app.global::<LibraryState>();
    let row = collection_slot(
        detail.clone(),
        Bucket::Grid,
        &mut images.borrow_mut(),
        &HashMap::new(),
        &loaded.summaries,
    );
    let info = summarize_members(
        &loaded.tracks,
        cider_offline_core::DownloadFormat::default(),
    );
    state.set_collection(row.row.id.clone());
    state.set_detail_open(true);
    state.set_detail_title(row.row.name.clone());
    state.set_detail_subtitle(detail_subtitle(&row.row).into());
    state.set_detail_kind(row.row.kind.clone());
    state.set_detail_is_custom(row.row.is_custom);
    state.set_detail_meta(
        format::collection_meta(info.songs, info.total_ms, row.row.is_custom).into(),
    );
    let (status, progress, downloaded) = match options.dl_state.as_deref() {
        Some("downloading") => ("downloading", 0.4, false),
        Some("paused") => ("paused", 0.4, false),
        Some("failed") => ("failed", 0.0, false),
        Some("done") => ("", 0.0, true),
        _ => ("", 0.0, row.row.downloaded),
    };
    state.set_detail_dl_status(status.into());
    state.set_detail_dl_progress(progress);
    state.set_detail_downloaded(downloaded);
    state.set_detail_partial(row.row.partial && options.dl_state.is_none());
    state.set_detail_elsewhere(row.row.elsewhere && options.dl_state.is_none());
    state.set_detail_dl_done(row.row.dl_done);
    state.set_detail_dl_total(row.row.dl_total);
    if let Some(url) = detail.collection.artwork_url.clone() {
        let request = ArtRequest::new(url, Bucket::Hero);
        let decoded = rt.block_on(artwork::load(library.clone(), vec![request]));
        if let Some(image) = images.borrow_mut().accept(decoded).into_values().next() {
            state.set_detail_artwork(image);
            state.set_detail_has_artwork(true);
        }
    }
}
