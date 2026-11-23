//! Downloads page: enqueue Apple Music URLs and watch the queue.
//!
//! The UI model mirrors the queue actor: catalog resolution + enqueue runs on
//! tokio through [`AppContext::with_backend`], and every mutation is driven
//! by [`DownloadEvent`]s forwarded from `runtime::Actor` as [`UiEvent`]s.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use cider_offline_core::storage::AudioVariant;
use cider_offline_core::{
    CatalogClient, DownloadEvent, DownloadRecord, DownloadSourceKind, DownloadStage,
    DownloadStatus, album_version, records_from_catalog,
};
use slint::{ComponentHandle as _, Model as _, ModelRc, VecModel};

use super::{AppContext, DownloadRow, DownloadsState, format};

/// Label of the action offered on a music video that can't be downloaded yet.
const ALBUM_VERSION_ACTION: &str = "Download album version";

/// Track-list filter selected in the combo box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Filter {
    #[default]
    All,
    Active,
    Queued,
    Done,
    Failed,
}

impl Filter {
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Active,
            2 => Self::Queued,
            3 => Self::Done,
            4 => Self::Failed,
            _ => Self::All,
        }
    }
}

/// Downloads state owned by the UI thread.
#[derive(Default)]
pub struct DownloadsData {
    /// Visible (filtered) rows shown to Slint.
    rows: Rc<VecModel<DownloadRow>>,
    /// Every row the queue is aware of, in insertion order.
    all: RefCell<Vec<DownloadRow>>,
    /// `id` → position in `all`.
    index: RefCell<HashMap<String, usize>>,
    search: RefCell<String>,
    filter: Cell<Filter>,
}

/// Wires the Downloads callbacks.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<DownloadsState>();
    state.set_rows(ModelRc::from(Rc::clone(&ctx.downloads.rows)));

    let c = Rc::clone(ctx);
    state.on_enqueue_url(move |input| enqueue_url(&c, input.as_str()));
    let c = Rc::clone(ctx);
    state.on_search_changed(move |text| {
        *c.downloads.search.borrow_mut() = text.to_lowercase();
        apply_filter(&c);
    });
    let c = Rc::clone(ctx);
    state.on_filter_changed(move |index| {
        c.downloads.filter.set(Filter::from_index(index));
        apply_filter(&c);
    });
    let c = Rc::clone(ctx);
    state.on_retry(move |id| retry(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_pause(move |id| pause(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_resume(move |id| resume(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_cancel(move |id| cancel(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_download_album_version(move |id| download_album_version(&c, id.as_str()));
    let c = Rc::clone(ctx);
    state.on_clear_finished(move || clear_finished(&c));
    let c = Rc::clone(ctx);
    state.on_cancel_all(move || cancel_all(&c));
    let c = Rc::clone(ctx);
    state.on_refresh(move || refresh(&c));
    let c = Rc::clone(ctx);
    state.on_retry_failed(move || bulk(&c, Bulk::RetryFailed));
    let c = Rc::clone(ctx);
    state.on_pause_all(move || bulk(&c, Bulk::PauseAll));
    let c = Rc::clone(ctx);
    state.on_resume_all(move || bulk(&c, Bulk::ResumeAll));

    push_summary(ctx);
}

fn enqueue_url(ctx: &Rc<AppContext>, input: &str) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        show_message(ctx, "Paste an Apple Music URL or ID first.");
        return;
    }
    tracing::info!(target: "downloads", url = %trimmed, "enqueue requested");
    show_message(ctx, "Resolving…");
    let query = trimmed.to_owned();
    ctx.with_backend(
        move |backend| async move {
            let catalog = CatalogClient::new(backend.storage.clone());
            let item = catalog
                .resolve(&query)
                .await
                .map_err(|e| format!("Could not resolve link: {e}"))?;
            let records = records_from_catalog(&item);
            if records.is_empty() {
                return Err("Nothing to download — the link has no tracks.".to_owned());
            }
            let count = records.len();
            backend
                .downloads
                .enqueue(records)
                .map_err(|e| format!("Could not enqueue: {e}"))?;
            Ok::<usize, String>(count)
        },
        |ctx, result| match result {
            Some(Ok(count)) => show_message(ctx, &format!("Added {count} tracks to the queue.")),
            Some(Err(message)) => show_message(ctx, &message),
            None => show_message(ctx, "The proxy is not running."),
        },
    );
}

fn retry(ctx: &Rc<AppContext>, id: &str) {
    tracing::info!(target: "downloads", %id, "retry requested");
    let id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            backend
                .downloads
                .retry(&id)
                .map_err(|e| format!("Could not retry: {e}"))
        },
        report_error,
    );
}

fn pause(ctx: &Rc<AppContext>, id: &str) {
    tracing::info!(target: "downloads", %id, "pause requested");
    let id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            backend
                .downloads
                .pause(&id)
                .map_err(|e| format!("Could not pause: {e}"))
        },
        report_error,
    );
}

fn resume(ctx: &Rc<AppContext>, id: &str) {
    tracing::info!(target: "downloads", %id, "resume requested");
    let id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            backend
                .downloads
                .resume(&id)
                .map_err(|e| format!("Could not resume: {e}"))
        },
        report_error,
    );
}

fn cancel(ctx: &Rc<AppContext>, id: &str) {
    tracing::info!(target: "downloads", %id, "cancel requested");
    let id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            backend
                .downloads
                .cancel(&id)
                .map_err(|e| format!("Could not cancel: {e}"))
        },
        report_error,
    );
}

/// Queues the album version of the music video in row `id` (an explicit user
/// action: the song's edit can differ from the video's).
fn download_album_version(ctx: &Rc<AppContext>, id: &str) {
    tracing::info!(target: "downloads", %id, "album version requested");
    let id = id.to_owned();
    ctx.with_backend(
        move |backend| async move {
            backend
                .downloads
                .download_album_version(&id)
                .await
                .map_err(|e| format!("Could not queue the album version: {e}"))
        },
        |ctx, result| match result {
            Some(Ok(Some(_))) => show_message(ctx, "Added the album version to the queue."),
            Some(Ok(None)) => show_message(ctx, "No album version of this video is known."),
            Some(Err(message)) => show_message(ctx, &message),
            None => {}
        },
    );
}

/// Looks up which "Can't download yet" rows have an album version and offers
/// [`ALBUM_VERSION_ACTION`] on them.
fn offer_alternatives(ctx: &Rc<AppContext>, rows: Vec<(String, String)>) {
    if rows.is_empty() {
        return;
    }
    ctx.with_backend(
        move |backend| async move {
            let mut offered = Vec::new();
            for (id, track_id) in rows {
                if let Ok(Some(_)) = album_version(&backend.storage, &track_id).await {
                    offered.push(id);
                }
            }
            offered
        },
        |ctx, offered| {
            for id in offered.unwrap_or_default() {
                update_row(ctx, &id, |row| {
                    let changed = row.alt_action.as_str() != ALBUM_VERSION_ACTION;
                    row.alt_action = ALBUM_VERSION_ACTION.into();
                    changed
                });
            }
        },
    );
}

/// `(row id, track id)` of every "Can't download yet" record.
fn unavailable_ids<'a>(
    records: impl IntoIterator<Item = &'a DownloadRecord>,
) -> Vec<(String, String)> {
    records
        .into_iter()
        .filter(|record| record.status == DownloadStatus::Unavailable)
        .map(|record| (record.id.clone(), record.track_id.clone()))
        .collect()
}

fn clear_finished(ctx: &Rc<AppContext>) {
    tracing::info!(target: "downloads", "clear finished requested");
    ctx.with_backend(
        move |backend| async move {
            backend
                .downloads
                .clear_finished()
                .map_err(|e| format!("Could not clear finished downloads: {e}"))
        },
        report_error,
    );
}

/// The confirmation of cancelling the rows of songs `titles`: names them up
/// to [`format::MAX_NAMED_SONGS`], counts them beyond (`all` for "Cancel
/// all": "all 37 unfinished downloads").
pub(super) fn cancel_message(titles: &[String], all: bool) -> String {
    let count = titles.len();
    let what = if count <= format::MAX_NAMED_SONGS {
        format::songs(count, titles)
    } else if all {
        format!("all {count} unfinished downloads")
    } else {
        format!("{count} downloads")
    };
    format!(
        "Stop and remove {what} from the download queue? Songs that already finished downloading are kept."
    )
}

/// Song titles of the drawer rows `ids`.
pub(super) fn row_titles(ctx: &AppContext, ids: &[String]) -> Vec<String> {
    ctx.downloads
        .all
        .borrow()
        .iter()
        .filter(|row| ids.iter().any(|id| id == row.id.as_str()))
        .map(|row| row.title.to_string())
        .collect()
}

/// "Cancel all…": asks, then cancels every unfinished row. Finished
/// downloads stay.
fn cancel_all(ctx: &Rc<AppContext>) {
    let titles: Vec<String> = ctx
        .downloads
        .all
        .borrow()
        .iter()
        .filter(|row| row.status != "done")
        .map(|row| row.title.to_string())
        .collect();
    if titles.is_empty() {
        return;
    }
    let message = cancel_message(&titles, true);
    let c = Rc::clone(ctx);
    super::dialogs::confirm(
        ctx,
        super::dialogs::Prompt {
            title: "Cancel all downloads",
            message: &message,
            confirm: "Cancel all",
            destructive: true,
        },
        move || {
            tracing::info!(target: "downloads", "cancel all requested");
            c.with_backend(
                move |backend| async move {
                    backend
                        .downloads
                        .cancel_all()
                        .map_err(|e| format!("Could not cancel the downloads: {e}"))
                },
                |ctx, result| {
                    report_error(ctx, result);
                    super::library::refresh_pins(ctx);
                },
            );
        },
    );
}

/// Drawer-wide queue actions.
#[derive(Debug, Clone, Copy)]
enum Bulk {
    RetryFailed,
    PauseAll,
    ResumeAll,
}

fn bulk(ctx: &Rc<AppContext>, action: Bulk) {
    tracing::info!(target: "downloads", ?action, "bulk action requested");
    ctx.with_backend(
        move |backend| async move {
            let queue = &backend.downloads;
            match action {
                Bulk::RetryFailed => queue.retry_failed(),
                Bulk::PauseAll => queue.pause_all(),
                Bulk::ResumeAll => queue.resume_all(),
            }
            .map_err(|e| format!("Could not update downloads: {e}"))
        },
        report_error,
    );
}

/// Shared continuation for queue commands: only a `Some(Err)` is surfaced.
fn report_error(ctx: &Rc<AppContext>, result: Option<Result<(), String>>) {
    if let Some(Err(message)) = result {
        show_message(ctx, &message);
    }
}

fn refresh(ctx: &Rc<AppContext>) {
    tracing::info!(target: "downloads", "refresh requested");
    refresh_from_backend(ctx);
}

/// Re-reads every stored row from the database and repopulates the UI. Safe
/// to call before the proxy backs the UI — callbacks surface a hint instead.
pub fn refresh_from_backend(ctx: &Rc<AppContext>) {
    ctx.with_backend(
        move |backend| async move {
            backend
                .storage
                .list_downloads()
                .await
                .map_err(|e| format!("Could not load downloads: {e}"))
        },
        |ctx, result| match result {
            Some(Ok(rows)) => replace_rows(ctx, &rows),
            Some(Err(message)) => show_message(ctx, &message),
            None => {}
        },
    );
}

fn replace_rows(ctx: &Rc<AppContext>, rows: &[DownloadRecord]) {
    let converted: Vec<DownloadRow> = rows.iter().map(row_from_record).collect();
    reindex(&ctx.downloads, converted);
    apply_filter(ctx);
    push_summary(ctx);
    offer_alternatives(ctx, unavailable_ids(rows));
    let done: Vec<String> = rows
        .iter()
        .filter(|row| row.status == DownloadStatus::Done)
        .map(|row| row.track_id.clone())
        .collect();
    refresh_variants(ctx, done);
}

/// Re-reads which mixes the songs named by `keys` (download keys) have on
/// disk and shows them on the Downloads rows and the Library song rows.
pub(super) fn refresh_variants(ctx: &Rc<AppContext>, keys: Vec<String>) {
    if keys.is_empty() {
        return;
    }
    ctx.with_backend(
        move |backend| async move {
            let found = backend.storage.local_variants(keys.clone()).await;
            (keys, found)
        },
        |ctx, result| {
            let Some((keys, Ok(found))) = result else {
                return;
            };
            let spatial = |key: &str| {
                found
                    .get(key)
                    .is_some_and(|variants| variants.contains(&AudioVariant::Spatial))
            };
            for key in &keys {
                set_row_spatial(ctx, key, spatial(key));
            }
            super::library::apply_variants(ctx, &keys, &found);
        },
    );
}

/// Sets the spatial badge of every Downloads row of song `track_id`.
fn set_row_spatial(ctx: &Rc<AppContext>, track_id: &str, spatial: bool) {
    let data = &ctx.downloads;
    let mut changed = false;
    for row in data.all.borrow_mut().iter_mut() {
        if row.track_id == track_id && row.spatial != spatial {
            row.spatial = spatial;
            changed = true;
        }
    }
    if changed {
        patch_model(&data.rows, |row| {
            let hit = row.track_id == track_id && row.spatial != spatial;
            if hit {
                row.spatial = spatial;
            }
            hit
        });
    }
}

/// Routes a [`DownloadEvent`] from the queue to the UI model.
pub fn on_event(ctx: &Rc<AppContext>, event: DownloadEvent) {
    match event {
        DownloadEvent::Added(record) => {
            upsert_row(ctx, row_from_record(&record));
            offer_alternatives(ctx, unavailable_ids([&record]));
        }
        DownloadEvent::StatusChanged {
            id,
            status,
            progress,
            message,
            stage,
            attempt: _,
        } => {
            let status_str = status_to_str(status);
            let detail = detail_line(status, progress, stage, message.as_deref());
            let mut track_id = None;
            update_row(ctx, &id, |row| {
                let message = message.clone().unwrap_or_default();
                let changed = row.status.as_str() != status_str
                    || (row.progress - progress).abs() > f32::EPSILON
                    || row.message.as_str() != message
                    || row.detail.as_str() != detail;
                row.status = status_str.into();
                row.progress = progress;
                row.message = message.into();
                row.detail = detail.as_str().into();
                track_id = Some(row.track_id.to_string());
                changed
            });
            match (status, track_id) {
                (DownloadStatus::Done, Some(track_id)) => {
                    super::library::mark_audio(ctx, std::slice::from_ref(&track_id), true);
                    refresh_variants(ctx, vec![track_id]);
                }
                (DownloadStatus::Unavailable, Some(track_id)) => {
                    offer_alternatives(ctx, vec![(id, track_id)]);
                }
                _ => {}
            }
        }
        DownloadEvent::Removed(id) => remove_row(ctx, &id),
        DownloadEvent::AudioRemoved(ids) => {
            for id in &ids {
                set_row_spatial(ctx, id, false);
            }
            super::library::mark_audio(ctx, &ids, false);
        }
        // The UI recomputes its counters from its own rows (`push_summary`),
        // which also covers the batch progress line.
        DownloadEvent::SummaryChanged { .. } => {}
    }
}

/// The one-line state of a download row shown in the drawer.
fn detail_line(
    status: DownloadStatus,
    progress: f32,
    stage: Option<DownloadStage>,
    message: Option<&str>,
) -> String {
    let percent = percent(progress);
    match status {
        DownloadStatus::Queued => message.map_or_else(|| "Waiting".to_owned(), short_reason),
        DownloadStatus::Downloading => {
            let label = stage.map_or("Downloading", DownloadStage::label);
            format!("{label}, {percent}%")
        }
        DownloadStatus::Paused => format!("Paused at {percent}%"),
        DownloadStatus::Done => "Downloaded".to_owned(),
        DownloadStatus::Failed => match message {
            Some(message) => format!("Failed: {}", short_reason(message)),
            None => "Failed".to_owned(),
        },
        DownloadStatus::Unavailable => match message {
            Some(message) => format!("Can't download yet: {}", short_reason(message)),
            None => "Can't download yet".to_owned(),
        },
    }
}

/// First line of an error, trimmed to a readable length.
fn short_reason(message: &str) -> String {
    const MAX: usize = 90;
    let line = message.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let cut: String = line.chars().take(MAX).collect();
    format!("{}…", cut.trim_end())
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a clamped 0..=100 percentage"
)]
fn percent(progress: f32) -> u8 {
    (progress.clamp(0.0, 1.0) * 100.0).round() as u8
}

fn status_to_str(status: DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Queued => "queued",
        DownloadStatus::Downloading => "downloading",
        DownloadStatus::Done => "done",
        DownloadStatus::Failed => "failed",
        DownloadStatus::Paused => "paused",
        DownloadStatus::Unavailable => "unavailable",
    }
}

fn source_label(kind: DownloadSourceKind, id: Option<&str>) -> String {
    let kind_str = match kind {
        DownloadSourceKind::Album => "album",
        DownloadSourceKind::Playlist => "playlist",
        DownloadSourceKind::Song => "song",
    };
    match id {
        Some(id) => format!("{kind_str}:{id}"),
        None => kind_str.to_owned(),
    }
}

fn row_from_record(record: &DownloadRecord) -> DownloadRow {
    DownloadRow {
        id: record.id.clone().into(),
        track_id: record.track_id.clone().into(),
        title: record.title.clone().into(),
        artist: record.artist.clone().into(),
        album: record.album.clone().unwrap_or_default().into(),
        status: status_to_str(record.status).into(),
        progress: record.progress,
        message: record.message.clone().unwrap_or_default().into(),
        detail: detail_line(
            record.status,
            record.progress,
            None,
            record.message.as_deref(),
        )
        .into(),
        source: source_label(record.source_kind, record.source_id.as_deref()).into(),
        source_name: record.source_name.clone().unwrap_or_default().into(),
        ..DownloadRow::default()
    }
}

// Filter + search logic (mirrors the pattern in `library.rs`).

fn matches(row: &DownloadRow, search: &str, filter: Filter) -> bool {
    let passes_filter = match filter {
        Filter::All => true,
        Filter::Active => row.status == "downloading" || row.status == "paused",
        Filter::Queued => row.status == "queued",
        Filter::Done => row.status == "done",
        Filter::Failed => row.status == "failed" || row.status == "unavailable",
    };
    passes_filter
        && (search.is_empty()
            || [&row.title, &row.artist, &row.album, &row.source_name]
                .iter()
                .any(|field| field.to_lowercase().contains(search)))
}

fn apply_filter(ctx: &AppContext) {
    let data = &ctx.downloads;
    let search = data.search.borrow();
    let visible: Vec<DownloadRow> = data
        .all
        .borrow()
        .iter()
        .filter(|row| matches(row, &search, data.filter.get()))
        .cloned()
        .collect();
    data.rows.set_vec(visible);
}

/// Applies `change` to every row of `model`, writing back rows it modified.
fn patch_model<T: Clone + 'static>(model: &VecModel<T>, mut change: impl FnMut(&mut T) -> bool) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i)
            && change(&mut row)
        {
            model.set_row_data(i, row);
        }
    }
}

fn reindex(data: &DownloadsData, rows: Vec<DownloadRow>) {
    let mut index = HashMap::with_capacity(rows.len());
    for (position, row) in rows.iter().enumerate() {
        index.insert(row.id.to_string(), position);
    }
    *data.all.borrow_mut() = rows;
    *data.index.borrow_mut() = index;
}

// Public helpers used by `on_event` to apply queue events to the UI model.

/// Inserts `row` or replaces an existing row with the same `id`.
pub fn upsert_row(ctx: &Rc<AppContext>, row: DownloadRow) {
    let data = &ctx.downloads;
    let id = row.id.to_string();
    let position = data.index.borrow().get(&id).copied();
    if let Some(pos) = position {
        if let Some(slot) = data.all.borrow_mut().get_mut(pos) {
            *slot = row.clone();
        }
        patch_model(&data.rows, |existing| {
            if existing.id == row.id {
                *existing = row.clone();
                true
            } else {
                false
            }
        });
    } else {
        let position = data.all.borrow().len();
        data.all.borrow_mut().push(row);
        data.index.borrow_mut().insert(id, position);
        apply_filter(ctx);
    }
    push_summary(ctx);
}

/// Mutates the row identified by `id` in place.
///
/// The closure returns `true` when the row changed, matching the
/// `patch_model` convention. Rows that do not exist are ignored.
pub fn update_row(
    ctx: &Rc<AppContext>,
    id: &str,
    mut change: impl FnMut(&mut DownloadRow) -> bool,
) {
    let data = &ctx.downloads;
    let mut modified = false;
    if let Some(&position) = data.index.borrow().get(id)
        && let Some(row) = data.all.borrow_mut().get_mut(position)
        && change(row)
    {
        modified = true;
    }
    if modified {
        patch_model(&data.rows, |row| {
            if row.id.as_str() == id {
                change(row)
            } else {
                false
            }
        });
        push_summary(ctx);
    }
}

/// Removes the row with `id`, if any.
pub fn remove_row(ctx: &Rc<AppContext>, id: &str) {
    let data = &ctx.downloads;
    let had = data.index.borrow().contains_key(id);
    if !had {
        return;
    }
    let kept: Vec<DownloadRow> = data
        .all
        .borrow()
        .iter()
        .filter(|row| row.id.as_str() != id)
        .cloned()
        .collect();
    reindex(data, kept);
    apply_filter(ctx);
    push_summary(ctx);
}

/// Download counters of the rows in the drawer.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Summary {
    downloading: usize,
    paused: usize,
    queued: usize,
    done: usize,
    failed: usize,
    unavailable: usize,
    /// Sum of per-row progress over the batch (done rows count 1.0).
    progress: f32,
}

impl Summary {
    pub(super) fn of(rows: &[DownloadRow]) -> Self {
        let mut summary = Self::default();
        for row in rows {
            match row.status.as_str() {
                "downloading" => {
                    summary.downloading += 1;
                    summary.progress += row.progress;
                }
                "paused" => {
                    summary.paused += 1;
                    summary.progress += row.progress;
                }
                "queued" => summary.queued += 1,
                "done" => {
                    summary.done += 1;
                    summary.progress += 1.0;
                }
                "failed" => summary.failed += 1,
                "unavailable" => summary.unavailable += 1,
                _ => {}
            }
        }
        summary
    }

    /// Rows that belong to the batch (everything that can be downloaded).
    fn batch(&self) -> usize {
        self.downloading + self.paused + self.queued + self.done + self.failed
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "row counts are far below f32's exact integer range"
    )]
    fn fraction(&self) -> f32 {
        match self.batch() {
            0 => 0.0,
            batch => (self.progress / batch as f32).clamp(0.0, 1.0),
        }
    }

    /// "12 of 40 songs, 3 downloading, 2 failed" (+ paused / can't download yet).
    fn line(&self) -> String {
        let batch = self.batch();
        if batch == 0 && self.unavailable == 0 {
            return String::new();
        }
        let noun = if batch == 1 { "song" } else { "songs" };
        let mut parts = vec![format!("{} of {batch} {noun}", self.done)];
        if self.downloading > 0 {
            parts.push(format!("{} downloading", self.downloading));
        }
        if self.paused > 0 {
            parts.push(format!("{} paused", self.paused));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.unavailable > 0 {
            parts.push(format!("{} can't download yet", self.unavailable));
        }
        parts.join(", ")
    }
}

fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Pushes the drawer counters, batch progress and summary line to the UI.
pub(super) fn set_summary(ui: &super::AppWindow, summary: Summary) {
    let state = ui.global::<DownloadsState>();
    state.set_active_count(count(summary.downloading + summary.paused));
    state.set_paused_count(count(summary.paused));
    state.set_queued_count(count(summary.queued));
    state.set_done_count(count(summary.done));
    state.set_failed_count(count(summary.failed));
    state.set_unavailable_count(count(summary.unavailable));
    state.set_total(count(summary.batch() + summary.unavailable));
    state.set_batch_progress(summary.fraction());
    state.set_summary_line(summary.line().into());
}

/// Live per-track download state keyed by catalog/track id, for the inline
/// indicators on the Library page. The queue uses the track's catalog id as the
/// row id, so this also matches a [`DownloadEvent::StatusChanged`] directly.
pub(super) fn track_progress(ctx: &AppContext) -> HashMap<String, (String, f32)> {
    ctx.downloads
        .all
        .borrow()
        .iter()
        .map(|row| {
            (
                row.track_id.to_string(),
                (row.status.to_string(), row.progress),
            )
        })
        .collect()
}

/// Live aggregate download state keyed by collection (album / playlist) id,
/// derived by grouping queue rows on their `source` tag ("album:ID" /
/// "playlist:ID"). Used for the inline indicator on the Library grid cards and
/// the collection detail's "Download all" control.
pub(super) fn collection_progress(ctx: &AppContext) -> HashMap<String, (String, f32)> {
    let mut groups: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    for row in ctx.downloads.all.borrow().iter() {
        if let Some((kind, id)) = row.source.as_str().split_once(':')
            && (kind == "album" || kind == "playlist")
            && !id.is_empty()
            // A "Can't download yet" song never makes its collection look failed.
            && row.status != "unavailable"
        {
            groups
                .entry(id.to_owned())
                .or_default()
                .push((row.status.to_string(), row.progress));
        }
    }
    groups
        .into_iter()
        .map(|(id, rows)| (id, aggregate_status(&rows)))
        .collect()
}

/// A queue command applied to several rows (a collection's songs, or the
/// click on a collection's aggregate download ring).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueAction {
    Pause,
    Resume,
    Retry,
    Cancel,
}

impl QueueAction {
    /// The ring click's action for an aggregate status, or `None` when
    /// nothing is in flight.
    fn for_status(status: &str) -> Option<Self> {
        match status {
            "downloading" | "queued" => Some(Self::Pause),
            "paused" => Some(Self::Resume),
            "failed" => Some(Self::Retry),
            _ => None,
        }
    }

    /// Whether a row in `status` is affected by this action.
    fn applies_to(self, status: &str) -> bool {
        match self {
            Self::Pause => matches!(status, "downloading" | "queued"),
            Self::Resume => status == "paused",
            Self::Retry => status == "failed",
            Self::Cancel => status != "done",
        }
    }
}

/// Ids of the drawer rows of songs `keys` (download keys) that `action`
/// applies to.
pub(super) fn rows_for(ctx: &AppContext, keys: &[String], action: QueueAction) -> Vec<String> {
    ctx.downloads
        .all
        .borrow()
        .iter()
        .filter(|row| {
            keys.iter()
                .any(|key| key == row.track_id.as_str() || key == row.id.as_str())
                && action.applies_to(row.status.as_str())
        })
        .map(|row| row.id.to_string())
        .collect()
}

/// Sends `action` for every row in `ids`.
pub(super) fn apply(ctx: &Rc<AppContext>, action: QueueAction, ids: Vec<String>) {
    tracing::info!(target: "downloads", ?action, count = ids.len(), "queue action requested");
    ctx.with_backend(
        move |backend| async move {
            for id in &ids {
                match action {
                    QueueAction::Pause => backend.downloads.pause(id),
                    QueueAction::Resume => backend.downloads.resume(id),
                    QueueAction::Retry => backend.downloads.retry(id),
                    QueueAction::Cancel => backend.downloads.cancel(id),
                }
                .map_err(|e| format!("Could not update downloads: {e}"))?;
            }
            Ok(())
        },
        move |ctx, result| {
            report_error(ctx, result);
            if action == QueueAction::Cancel {
                // Cancelled songs may have released their collection's pin.
                super::library::refresh_pins(ctx);
            }
        },
    );
}

/// Click-to-pause / click-to-resume (or retry, when failed) for a whole
/// album / playlist: applies the action matching the collection's aggregate
/// status to every queued member row tagged with `collection_id`.
pub(super) fn toggle_collection(ctx: &Rc<AppContext>, collection_id: &str) {
    let members: Vec<(String, String)> = ctx
        .downloads
        .all
        .borrow()
        .iter()
        .filter(|row| {
            row.source
                .as_str()
                .split_once(':')
                .is_some_and(|(kind, id)| {
                    (kind == "album" || kind == "playlist") && id == collection_id
                })
        })
        .map(|row| (row.id.to_string(), row.status.to_string()))
        .collect();
    let pairs: Vec<(String, f32)> = members.iter().map(|(_, s)| (s.clone(), 0.0)).collect();
    let (status, _) = aggregate_status(&pairs);
    let Some(action) = QueueAction::for_status(&status) else {
        return;
    };
    let ids: Vec<String> = members
        .into_iter()
        .filter(|(_, status)| action.applies_to(status))
        .map(|(id, _)| id)
        .collect();
    tracing::info!(target: "downloads", collection = %collection_id, ?action, count = ids.len(), "collection toggle requested");
    apply(ctx, action, ids);
}

/// Collapses a collection's per-track `(status, progress)` pairs into one
/// representative `(status, progress)` for its inline indicator.
fn aggregate_status(rows: &[(String, f32)]) -> (String, f32) {
    if rows.is_empty() {
        return (String::new(), 0.0);
    }
    let total: f32 = rows
        .iter()
        .map(|(status, progress)| if status == "done" { 1.0 } else { *progress })
        .sum();
    // A collection's track count is tiny, so the usize→f32 cast is lossless.
    #[expect(
        clippy::cast_precision_loss,
        reason = "collection track counts never exceed f32's 24-bit integer range"
    )]
    let average = total / rows.len() as f32;
    let has = |want: &str| rows.iter().any(|(status, _)| status == want);
    // Any in-flight / pending / failed work dominates a "done" sibling.
    // Once some songs are done, waiting songs still count as a batch in
    // progress: between two songs no row is "downloading", and a "queued"
    // collection would draw its ring at 0 % until the next one starts.
    let started = rows
        .iter()
        .any(|(status, progress)| status == "done" || *progress > 0.0);
    let status = if has("downloading") || (has("queued") && started) {
        "downloading"
    } else if has("queued") {
        "queued"
    } else if has("paused") {
        "paused"
    } else if has("failed") {
        "failed"
    } else {
        "done"
    };
    (status.to_owned(), average)
}

/// Sets the message line at the top of the Downloads page.
pub fn show_message(ctx: &AppContext, message: &str) {
    ctx.with_ui(|ui| ui.global::<DownloadsState>().set_message(message.into()));
}

/// Recomputes the counters and batch progress from the rows held in memory.
fn push_summary(ctx: &Rc<AppContext>) {
    let summary = Summary::of(&ctx.downloads.all.borrow());
    ctx.with_ui(|ui| set_summary(ui, summary));
}

#[cfg(test)]
mod tests {
    fn rows(pairs: &[(&str, f32)]) -> Vec<(String, f32)> {
        pairs.iter().map(|(s, p)| ((*s).to_owned(), *p)).collect()
    }

    fn titles(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("Song {n}")).collect()
    }

    #[test]
    fn cancel_message_should_name_up_to_five_songs() {
        assert_eq!(
            super::cancel_message(&titles(2), true),
            "Stop and remove “Song 1” and “Song 2” from the download queue? Songs that already \
             finished downloading are kept."
        );
    }

    #[test]
    fn cancel_message_should_count_more_than_five_songs() {
        assert_eq!(
            [
                super::cancel_message(&titles(6), true),
                super::cancel_message(&titles(6), false)
            ],
            [
                "Stop and remove all 6 unfinished downloads from the download queue? Songs that \
                 already finished downloading are kept."
                    .to_owned(),
                "Stop and remove 6 downloads from the download queue? Songs that already \
                 finished downloading are kept."
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn collection_with_done_and_waiting_songs_should_keep_showing_its_progress() {
        // Between two songs nothing is "downloading"; the ring must not drop
        // to the queued look (0 %) and back on every song.
        let between = super::aggregate_status(&rows(&[
            ("done", 1.0),
            ("done", 0.0),
            ("queued", 0.0),
            ("queued", 0.0),
        ]));
        let during = super::aggregate_status(&rows(&[
            ("done", 1.0),
            ("done", 0.0),
            ("downloading", 0.0),
            ("queued", 0.0),
        ]));
        assert_eq!(between, ("downloading".to_owned(), 0.5));
        assert_eq!(during, between);
    }

    #[test]
    fn collection_that_has_not_started_should_be_queued() {
        let (status, progress) =
            super::aggregate_status(&rows(&[("queued", 0.0), ("queued", 0.0)]));
        assert_eq!((status.as_str(), progress), ("queued", 0.0));
    }

    use super::*;

    fn row(id: &str, title: &str, status: &str) -> DownloadRow {
        DownloadRow {
            id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            status: status.into(),
            ..DownloadRow::default()
        }
    }

    #[test]
    fn search_should_match_case_insensitively() {
        assert!(matches(
            &row("1", "Hello World", "queued"),
            "world",
            Filter::All
        ));
    }

    #[test]
    fn active_filter_should_cover_downloading_and_paused() {
        assert!(matches(&row("1", "A", "downloading"), "", Filter::Active));
        assert!(matches(&row("2", "B", "paused"), "", Filter::Active));
        assert!(!matches(&row("3", "C", "queued"), "", Filter::Active));
    }

    #[test]
    fn failed_filter_should_only_match_failed() {
        assert!(matches(&row("1", "A", "failed"), "", Filter::Failed));
        assert!(!matches(&row("2", "B", "done"), "", Filter::Failed));
    }

    #[test]
    fn collection_toggle_should_pause_while_downloading() {
        assert_eq!(
            QueueAction::for_status("downloading"),
            Some(QueueAction::Pause)
        );
    }

    #[test]
    fn collection_toggle_should_resume_when_paused() {
        assert_eq!(QueueAction::for_status("paused"), Some(QueueAction::Resume));
    }

    #[test]
    fn collection_toggle_should_do_nothing_when_done() {
        assert_eq!(QueueAction::for_status("done"), None);
    }

    #[test]
    fn summary_should_exclude_unavailable_from_the_batch() {
        let rows = [
            row("1", "A", "done"),
            row("2", "B", "downloading"),
            row("3", "C", "failed"),
            row("4", "D", "unavailable"),
        ];
        assert_eq!(
            Summary::of(&rows).line(),
            "1 of 3 songs, 1 downloading, 1 failed, 1 can't download yet"
        );
    }

    #[test]
    fn detail_should_show_stage_and_percent() {
        let line = detail_line(
            DownloadStatus::Downloading,
            0.42,
            Some(DownloadStage::Fetching),
            None,
        );
        assert_eq!(line, "Downloading, 42%");
    }

    #[test]
    fn cancel_should_apply_to_every_unfinished_row() {
        let applies = [
            "queued",
            "downloading",
            "paused",
            "failed",
            "unavailable",
            "done",
        ]
        .map(|status| QueueAction::Cancel.applies_to(status));
        assert_eq!(applies, [true, true, true, true, true, false]);
    }

    #[test]
    fn collection_pause_should_skip_finished_members() {
        assert!(!QueueAction::Pause.applies_to("done"));
    }
}
