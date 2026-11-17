//! Library Add / Edit dialog: create or edit custom songs, albums and
//! playlists with full metadata, artwork, audio (local file or a referenced
//! catalog id) and lyrics (local TTML/LRC file or an LRCLIB refetch).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use cider_offline_core::storage::{
    CollectionKind, CustomAudio, CustomAudioMode, NewCustomCollection, NewCustomTrack, Playlist,
    Track,
};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use super::{AddCollectionRow, AppContext, LibraryAddState, LibraryState, dialogs};
use crate::runtime::Backend;

/// Item type selected in the dialog's segmented switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Song,
    Album,
    Playlist,
}

impl Kind {
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Album,
            2 => Self::Playlist,
            _ => Self::Song,
        }
    }

    fn index(self) -> i32 {
        match self {
            Self::Song => 0,
            Self::Album => 1,
            Self::Playlist => 2,
        }
    }
}

/// Dialog state owned by the UI thread (picked file paths + edit context).
#[derive(Default)]
pub struct LibraryAddData {
    cover: RefCell<Option<PathBuf>>,
    artwork: RefCell<Option<PathBuf>>,
    audio: RefCell<Option<PathBuf>>,
    lyrics: RefCell<Option<PathBuf>>,
    /// Placement targets (collection id, is-custom), parallel to the dropdown.
    collection_ids: RefCell<Vec<(String, bool)>>,
    /// `Some(id)` while editing an existing item.
    edit_id: RefCell<Option<String>>,
}

/// Wires the Add/Edit dialog callbacks.
pub fn bind(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let state = ui.global::<LibraryAddState>();

    let c = Rc::clone(ctx);
    state.on_set_kind(move |index| {
        // Switching type clears any file pickers that no longer apply.
        c.library_add.audio.borrow_mut().take();
        c.library_add.lyrics.borrow_mut().take();
        c.with_ui(|ui| {
            let s = ui.global::<LibraryAddState>();
            s.set_kind(index);
            s.set_has_audio_file(false);
            s.set_has_lyrics_file(false);
            s.set_fetch_lyrics(false);
            s.set_message(SharedString::new());
        });
    });

    let c = Rc::clone(ctx);
    state.on_pick_cover(move || pick_image(&c, PickTarget::Cover));
    let c = Rc::clone(ctx);
    state.on_pick_artwork(move || pick_image(&c, PickTarget::Artwork));
    let c = Rc::clone(ctx);
    state.on_pick_audio(move || pick_audio(&c));
    let c = Rc::clone(ctx);
    state.on_pick_lyrics(move || pick_lyrics(&c));
    let c = Rc::clone(ctx);
    state.on_clear_lyrics(move || {
        c.library_add.lyrics.borrow_mut().take();
        c.with_ui(|ui| {
            let s = ui.global::<LibraryAddState>();
            s.set_has_lyrics_file(false);
            s.set_lyrics_name(SharedString::new());
        });
    });
    let c = Rc::clone(ctx);
    state.on_submit(move || submit(&c));
    let c = Rc::clone(ctx);
    state.on_cancel(move || close(&c));
}

/// Which image slot a picker fills.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    Cover,
    Artwork,
}

fn pick_image(ctx: &Rc<AppContext>, target: PickTarget) {
    let owned = Rc::clone(ctx);
    dialogs::pick_image_file(ctx, "Choose an image", move |file| {
        let ctx = owned;
        let Some(file) = file else { return };
        let name = file_name(&file);
        let cell = match target {
            PickTarget::Cover => &ctx.library_add.cover,
            PickTarget::Artwork => &ctx.library_add.artwork,
        };
        *cell.borrow_mut() = Some(file);
        ctx.with_ui(|ui| {
            let s = ui.global::<LibraryAddState>();
            if target == PickTarget::Cover {
                s.set_has_cover(true);
                s.set_cover_name(name.into());
            } else {
                s.set_has_artwork(true);
                s.set_artwork_name(name.into());
            }
        });
    });
}

fn pick_audio(ctx: &Rc<AppContext>) {
    let owned = Rc::clone(ctx);
    dialogs::pick_audio_file(ctx, "Choose the audio file for this song", move |file| {
        let ctx = owned;
        let Some(file) = file else { return };
        let name = file_name(&file);
        *ctx.library_add.audio.borrow_mut() = Some(file);
        ctx.with_ui(|ui| {
            let s = ui.global::<LibraryAddState>();
            s.set_has_audio_file(true);
            s.set_audio_name(name.into());
        });
    });
}

fn pick_lyrics(ctx: &Rc<AppContext>) {
    let owned = Rc::clone(ctx);
    dialogs::pick_lyrics_file(ctx, "Choose a TTML or LRC lyrics file", move |file| {
        let ctx = owned;
        let Some(file) = file else { return };
        let name = file_name(&file);
        *ctx.library_add.lyrics.borrow_mut() = Some(file);
        ctx.with_ui(|ui| {
            let s = ui.global::<LibraryAddState>();
            s.set_has_lyrics_file(true);
            s.set_lyrics_name(name.into());
        });
    });
}

/// Opens the dialog to create a new custom item (defaults to the song flow;
/// the user can switch to an album or playlist with the type segmented control).
pub(super) fn open_add(ctx: &Rc<AppContext>) {
    reset(ctx);
    *ctx.library_add.edit_id.borrow_mut() = None;
    populate_collections(ctx);
    // Preselect the open custom collection as the placement when the dialog is
    // launched from a collection detail's "Add song to this …" affordance.
    let preselect = ctx.ui.upgrade().and_then(|ui| {
        let state = ui.global::<LibraryState>();
        (state.get_detail_open() && state.get_detail_is_custom())
            .then(|| state.get_collection().to_string())
    });
    let index = preselect.and_then(|id| {
        ctx.library_add
            .collection_ids
            .borrow()
            .iter()
            .position(|(cid, is_custom)| *is_custom && *cid == id)
    });
    ctx.with_ui(|ui| {
        let s = ui.global::<LibraryAddState>();
        s.set_editing(false);
        s.set_kind(Kind::Song.index());
        if let Some(index) = index {
            s.set_placement(1);
            s.set_collection_index(i32::try_from(index).unwrap_or(0));
        }
        s.set_open(true);
    });
}

/// Opens the dialog to edit an existing custom item (song or collection).
pub(super) fn open_edit(ctx: &Rc<AppContext>, id: String) {
    reset(ctx);
    populate_collections(ctx);
    let is_collection = id.starts_with("local-al-") || id.starts_with("local-pl-");
    let fetch_id = id.clone();
    ctx.with_backend(
        move |backend| async move {
            if is_collection {
                let playlist = backend.storage.get_playlist(&fetch_id).await?;
                Ok::<_, cider_offline_core::StorageError>((Some(playlist), None))
            } else {
                let track = backend.storage.get_track(&fetch_id).await?;
                Ok((None, Some(track)))
            }
        },
        move |ctx, result| match result {
            Some(Ok((Some(Some(playlist)), _))) => fill_collection(ctx, &id, &playlist),
            Some(Ok((_, Some(Some(track))))) => fill_song(ctx, &id, &track),
            Some(Ok(_)) => show_message(ctx, "That item is no longer in the library."),
            Some(Err(error)) => show_message(ctx, &format!("Could not load the item: {error}")),
            None => show_message(ctx, "The proxy is not running."),
        },
    );
}

fn fill_collection(ctx: &Rc<AppContext>, id: &str, playlist: &Playlist) {
    *ctx.library_add.edit_id.borrow_mut() = Some(id.to_owned());
    let kind = match playlist.kind {
        CollectionKind::Album => Kind::Album,
        CollectionKind::Playlist => Kind::Playlist,
    };
    ctx.with_ui(|ui| {
        let s = ui.global::<LibraryAddState>();
        s.set_editing(true);
        s.set_kind(kind.index());
        s.set_title(playlist.name.as_str().into());
        s.set_artist(opt(playlist.artist.as_deref()));
        s.set_description(opt(playlist.description.as_deref()));
        s.set_genre(opt(playlist.genre.as_deref()));
        s.set_release_date(opt(playlist.release_date.as_deref()));
        s.set_has_cover(playlist.artwork_url.is_some());
        s.set_cover_name(file_name_opt(playlist.artwork_url.as_deref()));
        s.set_open(true);
    });
}

fn fill_song(ctx: &Rc<AppContext>, id: &str, track: &Track) {
    *ctx.library_add.edit_id.borrow_mut() = Some(id.to_owned());
    let audio_mode = match track.audio_mode {
        Some(CustomAudioMode::LocalCopy) => 1,
        Some(CustomAudioMode::Reference) => 2,
        None => 0,
    };
    ctx.with_ui(|ui| {
        let s = ui.global::<LibraryAddState>();
        s.set_editing(true);
        s.set_kind(Kind::Song.index());
        s.set_title(track.title.as_str().into());
        s.set_artist(track.artist.as_str().into());
        s.set_album(opt(track.album.as_deref()));
        s.set_album_artist(opt(track.album_artist.as_deref()));
        s.set_composer(opt(track.composer.as_deref()));
        s.set_genre(opt(track.genre.as_deref()));
        s.set_release_date(opt(track.release_date.as_deref()));
        s.set_isrc(opt(track.isrc.as_deref()));
        s.set_url(opt(track.url.as_deref()));
        s.set_track_number(num(track.track_number));
        s.set_disc_number(num(track.disc_number));
        s.set_content_rating(content_rating_index(track.content_rating.as_deref()));
        s.set_has_artwork(track.artwork_url.is_some());
        s.set_artwork_name(file_name_opt(track.artwork_url.as_deref()));
        s.set_audio_mode(audio_mode);
        s.set_reference_id(opt(track.reference_track_id.as_deref()));
        if let Some(real) = &track.real_album_id {
            s.set_placement(2);
            s.set_real_album_id(real.as_str().into());
        } else {
            s.set_placement(0);
        }
        s.set_open(true);
    });
}

/// Builds the placement dropdown from the collections already loaded by the
/// Library page (real and custom albums / playlists).
fn populate_collections(ctx: &Rc<AppContext>) {
    let rows = ctx.library.collection_rows_snapshot();
    let mut ids = Vec::with_capacity(rows.len());
    let mut items = Vec::with_capacity(rows.len());
    let mut labels = Vec::with_capacity(rows.len());
    for (id, name, subtitle, is_custom) in rows {
        let label = format!("{name} — {subtitle}");
        ids.push((id.clone(), is_custom));
        labels.push(SharedString::from(label.as_str()));
        items.push(AddCollectionRow {
            id: id.into(),
            label: label.into(),
            is_custom,
        });
    }
    *ctx.library_add.collection_ids.borrow_mut() = ids;
    ctx.with_ui(|ui| {
        let s = ui.global::<LibraryAddState>();
        s.set_collections(ModelRc::new(VecModel::from(items)));
        s.set_collections_labels(ModelRc::new(VecModel::from(labels)));
        s.set_collection_index(0);
    });
}

/// Resets every form field and picked path to its default.
fn reset(ctx: &Rc<AppContext>) {
    ctx.library_add.cover.borrow_mut().take();
    ctx.library_add.artwork.borrow_mut().take();
    ctx.library_add.audio.borrow_mut().take();
    ctx.library_add.lyrics.borrow_mut().take();
    ctx.with_ui(|ui| {
        let s = ui.global::<LibraryAddState>();
        let blank = SharedString::new;
        s.set_message(blank());
        s.set_busy(false);
        s.set_title(blank());
        s.set_artist(blank());
        s.set_album(blank());
        s.set_album_artist(blank());
        s.set_composer(blank());
        s.set_description(blank());
        s.set_genre(blank());
        s.set_release_date(blank());
        s.set_isrc(blank());
        s.set_url(blank());
        s.set_track_number(blank());
        s.set_disc_number(blank());
        s.set_real_album_id(blank());
        s.set_new_album_name(blank());
        s.set_reference_id(blank());
        s.set_cover_name(blank());
        s.set_artwork_name(blank());
        s.set_audio_name(blank());
        s.set_lyrics_name(blank());
        s.set_content_rating(0);
        s.set_placement(0);
        s.set_audio_mode(0);
        s.set_collection_index(0);
        s.set_has_cover(false);
        s.set_has_artwork(false);
        s.set_has_audio_file(false);
        s.set_has_lyrics_file(false);
        s.set_fetch_lyrics(false);
    });
}

fn close(ctx: &Rc<AppContext>) {
    ctx.with_ui(|ui| ui.global::<LibraryAddState>().set_open(false));
}

/// All fields collected off the dialog, ready to persist.
struct Submission {
    editing: bool,
    edit_id: Option<String>,
    kind: Kind,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    composer: Option<String>,
    description: Option<String>,
    genre: Option<String>,
    release_date: Option<String>,
    isrc: Option<String>,
    url: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    content_rating: Option<String>,
    artwork_url: Option<String>,
    placement: i32,
    collection: Option<(String, bool)>,
    real_album_id: Option<String>,
    new_album_name: Option<String>,
    audio_mode: i32,
    reference_id: Option<String>,
    audio_file: Option<PathBuf>,
    lyrics_file: Option<PathBuf>,
    fetch_lyrics: bool,
}

fn submit(ctx: &Rc<AppContext>) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let s = ui.global::<LibraryAddState>();
    let kind = Kind::from_index(s.get_kind());

    let title = s.get_title().trim().to_owned();
    if title.is_empty() {
        show_message(ctx, "A title is required.");
        return;
    }
    let artist = opt_text(&s.get_artist());
    if matches!(kind, Kind::Song | Kind::Album) && artist.is_none() {
        show_message(ctx, "An artist is required.");
        return;
    }

    let collection = {
        let index = usize::try_from(s.get_collection_index().max(0)).unwrap_or(0);
        ctx.library_add.collection_ids.borrow().get(index).cloned()
    };
    let placement = s.get_placement();
    if kind == Kind::Song {
        if placement == 1 && collection.is_none() {
            show_message(
                ctx,
                "Choose an album or playlist, or pick another placement.",
            );
            return;
        }
        if placement == 2 && opt_text(&s.get_real_album_id()).is_none() {
            show_message(ctx, "Enter the real album id or URL.");
            return;
        }
        if placement == 3 && opt_text(&s.get_new_album_name()).is_none() {
            show_message(ctx, "Name the new custom album.");
            return;
        }
        if s.get_audio_mode() == 2 && opt_text(&s.get_reference_id()).is_none() {
            show_message(ctx, "Enter the catalog track id to borrow audio from.");
            return;
        }
    }

    let artwork_url = match kind {
        Kind::Song => path_string(&ctx.library_add.artwork),
        Kind::Album | Kind::Playlist => path_string(&ctx.library_add.cover),
    };

    let submission = Submission {
        editing: s.get_editing(),
        edit_id: ctx.library_add.edit_id.borrow().clone(),
        kind,
        title,
        artist,
        album: opt_text(&s.get_album()),
        album_artist: opt_text(&s.get_album_artist()),
        composer: opt_text(&s.get_composer()),
        description: opt_text(&s.get_description()),
        genre: opt_text(&s.get_genre()),
        release_date: opt_text(&s.get_release_date()),
        isrc: opt_text(&s.get_isrc()),
        url: opt_text(&s.get_url()),
        track_number: parse_u32(&s.get_track_number()),
        disc_number: parse_u32(&s.get_disc_number()),
        content_rating: content_rating_value(s.get_content_rating()),
        artwork_url,
        placement,
        collection,
        real_album_id: opt_text(&s.get_real_album_id()).map(|v| parse_album_id(&v)),
        new_album_name: opt_text(&s.get_new_album_name()),
        audio_mode: s.get_audio_mode(),
        reference_id: opt_text(&s.get_reference_id()),
        audio_file: ctx.library_add.audio.borrow().clone(),
        lyrics_file: ctx.library_add.lyrics.borrow().clone(),
        fetch_lyrics: s.get_fetch_lyrics(),
    };

    s.set_busy(true);
    show_message(ctx, "Saving…");
    ctx.with_backend(
        move |backend| async move { persist(&backend, submission).await },
        |ctx, result| {
            ctx.with_ui(|ui| ui.global::<LibraryAddState>().set_busy(false));
            match result {
                Some(Ok(())) => {
                    close(ctx);
                    super::library::refresh_after_change(ctx);
                }
                Some(Err(message)) => show_message(ctx, &message),
                None => show_message(ctx, "The proxy is not running."),
            }
        },
    );
}

/// Persists a submission: creates or updates the item and applies audio,
/// placement and lyrics. Runs on the tokio runtime.
async fn persist(backend: &Backend, sub: Submission) -> Result<(), String> {
    match sub.kind {
        Kind::Album | Kind::Playlist => persist_collection(backend, sub).await,
        Kind::Song => persist_song(backend, sub).await,
    }
}

async fn persist_collection(backend: &Backend, sub: Submission) -> Result<(), String> {
    let kind = if sub.kind == Kind::Album {
        CollectionKind::Album
    } else {
        CollectionKind::Playlist
    };
    // Preserve an existing cover when editing without picking a new one.
    let artwork_url = match (&sub.artwork_url, &sub.edit_id) {
        (Some(_), _) => sub.artwork_url.clone(),
        (None, Some(id)) if sub.editing => backend
            .storage
            .get_playlist(id)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|p| p.artwork_url),
        (None, _) => None,
    };
    let new = NewCustomCollection {
        kind,
        name: sub.title.clone(),
        artist: sub.artist.clone(),
        artwork_url,
        description: sub.description.clone(),
        genre: sub.genre.clone(),
        release_date: sub.release_date.clone(),
        metadata: None,
    };
    if sub.editing {
        let id = sub.edit_id.clone().ok_or("Missing item id.")?;
        backend
            .storage
            .update_custom_collection(&id, new)
            .await
            .map_err(|e| e.to_string())?;
    } else {
        backend
            .storage
            .create_custom_collection(new)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn persist_song(backend: &Backend, sub: Submission) -> Result<(), String> {
    // Real-album tagging (borrow identity) for placement 1-real or 2.
    let real_album_id = match sub.placement {
        1 => match &sub.collection {
            Some((id, is_custom)) if !is_custom => Some(id.clone()),
            _ => None,
        },
        2 => sub.real_album_id.clone(),
        _ => None,
    };

    // Audio to record now. A newly-picked local file is imported afterwards, so
    // the record starts with no local copy; on edit without a new file the
    // existing audio is preserved so an unrelated edit does not wipe it.
    let existing = match &sub.edit_id {
        Some(id) if sub.editing => backend
            .storage
            .get_track(id)
            .await
            .map_err(|e| e.to_string())?,
        _ => None,
    };
    let record_audio = match sub.audio_mode {
        2 => match &sub.reference_id {
            Some(id) => CustomAudio::Reference(id.clone()),
            None => CustomAudio::None,
        },
        1 if sub.audio_file.is_some() => CustomAudio::None,
        1 => existing_audio(existing.as_ref()),
        _ => CustomAudio::None,
    };

    let new = build_new_track(&sub, existing.as_ref(), real_album_id, record_audio);
    let id = if sub.editing {
        let id = sub.edit_id.clone().ok_or("Missing item id.")?;
        backend
            .storage
            .update_custom_track(&id, new)
            .await
            .map_err(|e| e.to_string())?;
        id
    } else {
        backend
            .storage
            .create_custom_track(new)
            .await
            .map_err(|e| e.to_string())?
    };
    apply_song_extras(backend, &sub, &id).await
}

/// Builds the custom-track record from the dialog fields, inheriting duration /
/// artwork / time-synced-lyrics state from an existing row when editing.
fn build_new_track(
    sub: &Submission,
    existing: Option<&Track>,
    real_album_id: Option<String>,
    audio: CustomAudio,
) -> NewCustomTrack {
    NewCustomTrack {
        title: sub.title.clone(),
        artist: sub.artist.clone().unwrap_or_default(),
        album: sub.album.clone(),
        album_artist: sub.album_artist.clone(),
        composer: sub.composer.clone(),
        genre: sub.genre.clone(),
        track_number: sub.track_number,
        disc_number: sub.disc_number,
        release_date: sub.release_date.clone(),
        isrc: sub.isrc.clone(),
        url: sub.url.clone(),
        duration_ms: existing.and_then(|t| t.duration_ms),
        artwork_url: sub
            .artwork_url
            .clone()
            .or_else(|| existing.and_then(|t| t.artwork_url.clone())),
        has_lyrics: sub.lyrics_file.is_some()
            || sub.fetch_lyrics
            || existing.is_some_and(|t| t.has_lyrics),
        has_time_synced_lyrics: existing.is_some_and(|t| t.has_time_synced_lyrics),
        content_rating: sub.content_rating.clone(),
        real_album_id,
        audio,
        metadata: None,
    }
}

/// Applies the post-create steps for a custom song: importing a picked local
/// audio file, collection membership, and lyrics (chosen file or LRCLIB).
async fn apply_song_extras(backend: &Backend, sub: &Submission, id: &str) -> Result<(), String> {
    // Import a newly-picked local audio file (copies it into the media dir).
    if sub.audio_mode == 1
        && let Some(file) = &sub.audio_file
    {
        backend
            .library
            .import_custom_audio(id, file)
            .await
            .map_err(|e| format!("Audio import failed: {e}"))?;
    }

    // Membership in a custom collection (existing custom, or a new album).
    match sub.placement {
        1 => {
            if let Some((cid, true)) = &sub.collection {
                backend
                    .storage
                    .place_track_in_collection(cid, id, usize::MAX)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        3 => {
            if let Some(name) = &sub.new_album_name {
                let album = backend
                    .storage
                    .create_custom_collection(NewCustomCollection {
                        kind: CollectionKind::Album,
                        name: name.clone(),
                        artist: sub.artist.clone(),
                        artwork_url: None,
                        description: None,
                        genre: None,
                        release_date: None,
                        metadata: None,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                backend
                    .storage
                    .place_track_in_collection(&album, id, usize::MAX)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        _ => {}
    }

    // Lyrics: a chosen file wins; otherwise an optional LRCLIB refetch.
    if let Some(file) = &sub.lyrics_file {
        backend
            .library
            .import_lyrics(id, file)
            .await
            .map_err(|e| format!("Lyrics import failed: {e}"))?;
    } else if sub.fetch_lyrics {
        backend
            .library
            .refresh_lyrics(id)
            .await
            .map_err(|e| format!("Lyrics lookup failed: {e}"))?;
    }
    Ok(())
}

fn existing_audio(track: Option<&Track>) -> CustomAudio {
    match track {
        Some(t) => match t.audio_mode {
            Some(CustomAudioMode::LocalCopy) => match &t.audio_path {
                Some(path) => CustomAudio::LocalCopy(path.clone()),
                None => CustomAudio::None,
            },
            Some(CustomAudioMode::Reference) => match &t.reference_track_id {
                Some(id) => CustomAudio::Reference(id.clone()),
                None => CustomAudio::None,
            },
            None => CustomAudio::None,
        },
        None => CustomAudio::None,
    }
}

// ----- small helpers ----------------------------------------------------------

fn show_message(ctx: &AppContext, message: &str) {
    ctx.with_ui(|ui| ui.global::<LibraryAddState>().set_message(message.into()));
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn file_name_str(value: &str) -> String {
    std::path::Path::new(value)
        .file_name()
        .map_or_else(|| value.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// Display file name for an optional stored path (cover / artwork).
fn file_name_opt(value: Option<&str>) -> SharedString {
    value.map(file_name_str).unwrap_or_default().into()
}

fn path_string(slot: &RefCell<Option<PathBuf>>) -> Option<String> {
    slot.borrow()
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
}

fn opt(value: Option<&str>) -> SharedString {
    value.unwrap_or("").into()
}

fn num(value: Option<u32>) -> SharedString {
    value.map(|v| v.to_string()).unwrap_or_default().into()
}

fn opt_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok().filter(|n| *n > 0)
}

fn content_rating_index(value: Option<&str>) -> i32 {
    match value {
        Some("clean") => 1,
        Some("explicit") => 2,
        _ => 0,
    }
}

fn content_rating_value(index: i32) -> Option<String> {
    match index {
        1 => Some("clean".to_owned()),
        2 => Some("explicit".to_owned()),
        _ => None,
    }
}

/// Extracts the numeric album id from an Apple album URL, or returns the input
/// trimmed when it is already a bare id.
fn parse_album_id(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    // A URL segment can carry a query (`?i=…`); drop it and prefer trailing digits.
    let segment = last.split('?').next().unwrap_or(last);
    let digits: String = segment.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        segment.to_owned()
    } else {
        digits
    }
}
