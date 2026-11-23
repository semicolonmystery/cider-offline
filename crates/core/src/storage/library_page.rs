//! Paged library queries for the UI: one window of collections or tracks plus
//! the total, so long lists load on scroll instead of all at once.
//!
//! - **Tracks** are paged in SQL (`LIMIT` / `OFFSET`, plus a `COUNT(*)` with
//!   the same `WHERE`). The source predicate, the missing-audio / missing-lyrics
//!   filter and the search all run in SQLite, ordered exactly like the flat list
//!   of [`Storage::library_contents`] (`artist, album, title`, with `id` as a
//!   final tie-break so page boundaries are stable). Inside an open collection
//!   the rows follow the collection's own order, joined through `json_each`.
//! - **Collections** need the Rust-side album grouping of
//!   [`super::library`] (union-find over identity keys, synthesized album
//!   groups), which SQL cannot express. The grouped view is therefore computed
//!   once and cached per filter, keyed by SQLite's `total_changes()` counter of
//!   the (single) connection, so it is rebuilt only after a write. Pages and
//!   searches are slices of that cached, ordered list.
//!
//! Search is case- and accent-insensitive on both sides (the `fold_text` SQL
//! function, the same folding as the Apple Music grid ordering).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, ToSql};

use super::library::{self, LibraryCollection, LibraryFilter, sort_fold};
use super::models::{CollectionKind, Track};
use super::paths::PathCodec;
use super::repo::{TRACK_COLUMN_COUNT, TRACK_COLUMNS, track_from_row};
use super::{Storage, StorageError};
use crate::DownloadFormat;
use crate::download_needs::CollectionDownload;

/// Track-list filter of the songs view and collection detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackFilter {
    /// Every track.
    #[default]
    All,
    /// Tracks without a local audio file.
    MissingAudio,
    /// Tracks without lyrics.
    MissingLyrics,
}

/// What one paged library query selects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryQuery {
    /// The library source (Apple Music / Downloaded / All / Custom).
    pub filter: LibraryFilter,
    /// Search text (matched case- and accent-insensitively); empty = none.
    pub search: String,
    /// Audio / lyrics filter for tracks (ignored for collections).
    pub tracks: TrackFilter,
    /// Restrict tracks to this collection's members, in its order (ignored
    /// for collections).
    pub collection: Option<String>,
}

/// One window of a paged query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPage<T> {
    /// The rows of `offset .. offset + items.len()`.
    pub items: Vec<T>,
    /// Index of the first row.
    pub offset: usize,
    /// Number of rows the whole query matches.
    pub total: usize,
}

/// A track row of a page, with the per-row facts the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryTrack {
    /// The track.
    pub track: Track,
    /// A user track-id → local file mapping exists.
    pub has_mapping: bool,
    /// Whether the song has a spatial mix, as the download queue learned it
    /// (`None`: not looked up; see [`crate::download_needs`]).
    pub spatial_known: Option<bool>,
}

impl LibraryTrack {
    /// Whether the song lacks a mix `format` wants (see
    /// [`crate::download_needs`]).
    #[must_use]
    pub fn needs_download(&self, format: crate::DownloadFormat) -> bool {
        crate::download_needs::track_needs_download(&self.track, format, self.spatial_known)
    }
}

/// Cached grouped collection view of one filter.
#[derive(Debug, Clone)]
pub(super) struct CachedView {
    /// `Connection::total_changes()` when it was built.
    changes: u64,
    collections: Arc<Vec<LibraryCollection>>,
}

/// Per-filter cache of the grouped collection view. Only touched while the
/// connection mutex is held, so it is always consistent with the database.
pub(super) type ViewCache = Arc<Mutex<HashMap<LibraryFilter, CachedView>>>;

/// Name of the SQL text-folding function registered on every connection.
const FOLD_FUNCTION: &str = "fold_text";

/// Registers `fold_text(text)`: trimmed, lower-cased, Latin diacritics
/// stripped ([`sort_fold`]); `NULL` folds to `''`.
pub(super) fn register_functions(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        FOLD_FUNCTION,
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: Option<String> = ctx.get(0)?;
            Ok(text.as_deref().map(sort_fold).unwrap_or_default())
        },
    )
}

/// The grouped collection view of `filter`, rebuilt only after a write.
fn cached_collections(
    conn: &Connection,
    codec: &PathCodec,
    cache: &ViewCache,
    filter: LibraryFilter,
) -> Result<Arc<Vec<LibraryCollection>>, StorageError> {
    let changes = conn.total_changes();
    {
        let views = cache.lock().map_err(|_| StorageError::Poisoned)?;
        if let Some(view) = views.get(&filter)
            && view.changes == changes
        {
            return Ok(Arc::clone(&view.collections));
        }
    }
    let collections = Arc::new(library::contents(conn, codec, filter)?.collections);
    cache.lock().map_err(|_| StorageError::Poisoned)?.insert(
        filter,
        CachedView {
            changes,
            collections: Arc::clone(&collections),
        },
    );
    Ok(collections)
}

/// Whether a collection matches the (already folded) search text: its name,
/// artist or kind label ("album" / "playlist").
fn collection_matches(entry: &LibraryCollection, folded: &str) -> bool {
    if folded.is_empty() {
        return true;
    }
    let collection = &entry.collection;
    let kind = match collection.kind {
        CollectionKind::Album => "album",
        CollectionKind::Playlist => "playlist",
    };
    sort_fold(&collection.name).contains(folded)
        || collection
            .artist
            .as_deref()
            .is_some_and(|artist| sort_fold(artist).contains(folded))
        || kind.contains(folded)
}

fn collections_page(
    conn: &Connection,
    codec: &PathCodec,
    cache: &ViewCache,
    query: &LibraryQuery,
    offset: usize,
    limit: usize,
) -> Result<LibraryPage<LibraryCollection>, StorageError> {
    let view = cached_collections(conn, codec, cache, query.filter)?;
    let folded = sort_fold(&query.search);
    let (items, total) = if folded.is_empty() {
        let end = offset.saturating_add(limit).min(view.len());
        let items = view.get(offset..end).map(<[_]>::to_vec).unwrap_or_default();
        (items, view.len())
    } else {
        let mut total = 0;
        let mut items = Vec::new();
        for entry in view.iter().filter(|e| collection_matches(e, &folded)) {
            if total >= offset && items.len() < limit {
                items.push(entry.clone());
            }
            total += 1;
        }
        (items, total)
    };
    Ok(LibraryPage {
        items,
        offset,
        total,
    })
}

/// Ordered member ids of a collection in the grouped view of `filter`.
fn members(
    conn: &Connection,
    codec: &PathCodec,
    cache: &ViewCache,
    filter: LibraryFilter,
    collection_id: &str,
) -> Result<Option<Vec<String>>, StorageError> {
    Ok(cached_collections(conn, codec, cache, filter)?
        .iter()
        .find(|entry| entry.collection.id == collection_id)
        .map(|entry| entry.track_ids.clone()))
}

/// The download summary of each of `ids` in the grouped view of `filter`,
/// counted over the same members the view lists (see
/// [`Storage::collection_downloads`]).
fn collection_downloads(
    conn: &Connection,
    codec: &PathCodec,
    cache: &ViewCache,
    filter: LibraryFilter,
    ids: &[String],
    format: DownloadFormat,
) -> Result<HashMap<String, CollectionDownload>, StorageError> {
    let view = cached_collections(conn, codec, cache, filter)?;
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let entries: Vec<&LibraryCollection> = view
        .iter()
        .filter(|entry| wanted.contains(entry.collection.id.as_str()))
        .collect();
    let mut songs: Vec<&str> = entries
        .iter()
        .flat_map(|entry| entry.track_ids.iter().map(String::as_str))
        .collect();
    songs.sort_unstable();
    songs.dedup();
    let facts = member_facts(conn, codec, &songs)?;
    let owners: Vec<&str> = entries
        .iter()
        .map(|entry| entry.collection.id.as_str())
        .collect();
    let pins = owner_pins(conn, &owners)?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let owner = entry.collection.id.as_str();
            let mut summary = CollectionDownload::default();
            let mut seen = HashSet::new();
            for id in &entry.track_ids {
                // A song listed twice counts once; a member with no stored
                // song can't be downloaded from here and is left out.
                if seen.insert(id.as_str())
                    && let Some((track, known)) = facts.get(id.as_str())
                {
                    let linked = pins
                        .get(owner)
                        .is_some_and(|songs| songs.contains(track.id.as_str()));
                    summary.add(track, format, *known, linked);
                }
            }
            (entry.collection.id.clone(), summary)
        })
        .collect())
}

/// The songs each of the collections `owners` pins, by owner.
fn owner_pins<'a>(
    conn: &Connection,
    owners: &[&'a str],
) -> Result<HashMap<&'a str, HashSet<String>>, StorageError> {
    let mut stmt = conn.prepare("SELECT track_id FROM download_pins WHERE owner = ?1")?;
    let mut pins = HashMap::with_capacity(owners.len());
    for &owner in owners {
        let songs = stmt
            .query_map([owner], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        pins.insert(owner, songs);
    }
    Ok(pins)
}

/// The stored songs `ids` with what the queue learned about their spatial
/// mix, by song id.
fn member_facts(
    conn: &Connection,
    codec: &PathCodec,
    ids: &[&str],
) -> Result<HashMap<String, (Track, Option<bool>)>, StorageError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let json = serde_json::to_string(ids)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS},
                (SELECT s.has_spatial FROM song_spatial s WHERE s.track_id = t.id)
         FROM json_each(:ids) j JOIN tracks t ON t.id = j.value"
    ))?;
    let rows = stmt.query_map(&[(":ids", &json)], |row| {
        let track = track_from_row(codec)(row)?;
        let known: Option<bool> = row.get(TRACK_COLUMN_COUNT)?;
        Ok((track.id.clone(), (track, known)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn track_filter_predicate(filter: TrackFilter) -> &'static str {
    match filter {
        TrackFilter::All => "1",
        TrackFilter::MissingAudio => {
            "NOT EXISTS (SELECT 1 FROM track_audio a WHERE a.track_id = t.id)"
        }
        TrackFilter::MissingLyrics => "t.lyrics_source = 'none'",
    }
}

fn tracks_page(
    conn: &Connection,
    codec: &PathCodec,
    cache: &ViewCache,
    query: &LibraryQuery,
    offset: usize,
    limit: usize,
) -> Result<LibraryPage<LibraryTrack>, StorageError> {
    let folded = sort_fold(&query.search);
    let members_json = match &query.collection {
        Some(id) => {
            let ids = members(conn, codec, cache, query.filter, id)?.unwrap_or_default();
            Some(serde_json::to_string(&ids)?)
        }
        None => None,
    };

    // Source: the collection's members (in their order) or the filter's
    // flat track set.
    let (from, scope, order) = match members_json {
        Some(_) => (
            "(SELECT tr.*, j.key AS member_pos FROM json_each(:members) j
              JOIN tracks tr ON tr.id = j.value) t",
            "1",
            "t.member_pos",
        ),
        None => (
            "tracks t",
            library::track_predicate(query.filter),
            "t.artist, t.album, t.title, t.id",
        ),
    };
    let search = if folded.is_empty() {
        "1"
    } else {
        "(instr(fold_text(t.title), :search) > 0
          OR instr(fold_text(t.artist), :search) > 0
          OR instr(fold_text(t.album), :search) > 0)"
    };
    let filter = track_filter_predicate(query.tracks);
    let condition = format!("({scope}) AND ({filter}) AND {search}");

    let mut params: Vec<(&str, &dyn ToSql)> = Vec::with_capacity(4);
    if let Some(json) = &members_json {
        params.push((":members", json));
    }
    if !folded.is_empty() {
        params.push((":search", &folded));
    }
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {from} WHERE {condition}"),
        params.as_slice(),
        |row| row.get(0),
    )?;

    let limit_param = i64::try_from(limit).unwrap_or(i64::MAX);
    let offset_param = i64::try_from(offset).unwrap_or(i64::MAX);
    params.push((":limit", &limit_param));
    params.push((":offset", &offset_param));
    let mapping_column = TRACK_COLUMN_COUNT;
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS},
                EXISTS (SELECT 1 FROM track_mappings m WHERE m.track_id = t.id),
                (SELECT s.has_spatial FROM song_spatial s
                 WHERE s.track_id = t.id)
         FROM {from} WHERE {condition}
         ORDER BY {order} LIMIT :limit OFFSET :offset"
    ))?;
    let items = stmt
        .query_map(params.as_slice(), |row| {
            Ok(LibraryTrack {
                track: track_from_row(codec)(row)?,
                has_mapping: row.get(mapping_column)?,
                spatial_known: row.get(mapping_column + 1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(LibraryPage {
        items,
        offset,
        total: usize::try_from(total).unwrap_or(0),
    })
}

impl Storage {
    /// One page of the collections grid of `query.filter`, in the same order
    /// as [`Storage::library_contents`], filtered by `query.search`.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn collections_page(
        &self,
        query: LibraryQuery,
        offset: usize,
        limit: usize,
    ) -> Result<LibraryPage<LibraryCollection>, StorageError> {
        let cache = Arc::clone(&self.views);
        let codec = self.codec.clone();
        self.call(move |c| collections_page(c, &codec, &cache, &query, offset, limit))
            .await
    }

    /// One page of tracks: the flat list of `query.filter` (ordered like
    /// [`Storage::library_contents`]) or, with `query.collection`, that
    /// collection's members in its own order; filtered by `query.tracks` and
    /// `query.search` in SQL.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn tracks_page(
        &self,
        query: LibraryQuery,
        offset: usize,
        limit: usize,
    ) -> Result<LibraryPage<LibraryTrack>, StorageError> {
        let cache = Arc::clone(&self.views);
        let codec = self.codec.clone();
        self.call(move |c| tracks_page(c, &codec, &cache, &query, offset, limit))
            .await
    }

    /// How much of each collection `ids` of the grid of `filter` is
    /// downloaded under `format`, counted from the audio files of the songs
    /// the view lists for it (the same members its detail shows). The
    /// collection's own pins only tell "downloaded" from "saved through
    /// other downloads"; queue rows and lyrics are not consulted.
    /// Collections not in the view are left out of the map.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn collection_downloads(
        &self,
        filter: LibraryFilter,
        ids: Vec<String>,
        format: DownloadFormat,
    ) -> Result<HashMap<String, CollectionDownload>, StorageError> {
        let cache = Arc::clone(&self.views);
        let codec = self.codec.clone();
        self.call(move |c| collection_downloads(c, &codec, &cache, filter, &ids, format))
            .await
    }

    /// Ordered member track ids of a collection as the grid of `filter`
    /// shows it (also for synthesized album groups); `None` if the
    /// collection is not part of that view.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn collection_members(
        &self,
        filter: LibraryFilter,
        collection_id: &str,
    ) -> Result<Option<Vec<String>>, StorageError> {
        let cache = Arc::clone(&self.views);
        let id = collection_id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| members(c, &codec, &cache, filter, &id))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        CatalogUpdate, LibrarySnapshot, Playlist, SyncedCollection, TrackMetadata,
    };
    use std::path::PathBuf;

    fn song(id: &str, title: &str, artist: &str, album: &str) -> TrackMetadata {
        TrackMetadata {
            id: id.to_owned(),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: Some(album.to_owned()),
            ..TrackMetadata::default()
        }
    }

    /// Ten songs (`i.0` … `i.9`) in one synced playlist, in reverse order.
    async fn store() -> Storage {
        let storage = Storage::open_in_memory().unwrap();
        let tracks: Vec<TrackMetadata> = (0..10)
            .map(|n| song(&format!("i.{n}"), &format!("Song {n}"), "Artist", "Album"))
            .collect();
        let order: Vec<String> = (0..10).rev().map(|n| format!("i.{n}")).collect();
        storage
            .replace_library(LibrarySnapshot {
                collections: vec![SyncedCollection {
                    collection: Playlist::minimal("p.1", CollectionKind::Playlist, "Mix"),
                    track_ids: order,
                    reported_total: Some(10),
                }],
                tracks,
                songs: Vec::new(),
            })
            .await
            .unwrap();
        storage
    }

    fn query() -> LibraryQuery {
        LibraryQuery {
            filter: LibraryFilter::AppleMusic,
            ..LibraryQuery::default()
        }
    }

    fn ids(page: &LibraryPage<LibraryTrack>) -> Vec<&str> {
        page.items.iter().map(|t| t.track.id.as_str()).collect()
    }

    /// One song in the synced library under two library ids: `i.x` in a
    /// saved album and `a.5` in a playlist, both with catalog id `5`.
    async fn twin_library() -> Storage {
        let storage = Storage::open_in_memory().unwrap();
        let twin = |id: &str| TrackMetadata {
            catalog_id: Some("5".to_owned()),
            ..song(id, "Song", "Artist", "Album")
        };
        storage
            .replace_library(LibrarySnapshot {
                collections: vec![
                    SyncedCollection {
                        collection: Playlist::minimal("l.1", CollectionKind::Album, "Album"),
                        track_ids: vec!["i.x".to_owned()],
                        reported_total: Some(1),
                    },
                    SyncedCollection {
                        collection: Playlist::minimal("p.1", CollectionKind::Playlist, "Mix"),
                        track_ids: vec!["a.5".to_owned()],
                        reported_total: Some(1),
                    },
                ],
                tracks: vec![twin("i.x"), twin("a.5")],
                songs: Vec::new(),
            })
            .await
            .unwrap();
        storage
    }

    #[tokio::test]
    async fn songs_list_should_show_a_song_listed_under_two_library_ids_once() {
        let page = twin_library()
            .await
            .tracks_page(query(), 0, 10)
            .await
            .unwrap();
        assert_eq!((page.total, ids(&page)), (1, vec!["5"]));
    }

    #[tokio::test]
    async fn collection_members_should_be_song_ids() {
        let storage = twin_library().await;
        let members = storage
            .collection_members(LibraryFilter::AppleMusic, "p.1")
            .await
            .unwrap();
        assert_eq!(members, Some(vec!["5".to_owned()]));
    }

    #[tokio::test]
    async fn tracks_page_should_report_total_independent_of_window() {
        let page = store().await.tracks_page(query(), 0, 3).await.unwrap();
        assert_eq!((page.items.len(), page.total), (3, 10));
    }

    #[tokio::test]
    async fn tracks_pages_should_concatenate_to_full_ordered_list() {
        let storage = store().await;
        let mut paged = Vec::new();
        for offset in (0..10).step_by(3) {
            let page = storage.tracks_page(query(), offset, 3).await.unwrap();
            paged.extend(page.items.into_iter().map(|t| t.track.id));
        }
        let full: Vec<String> = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap()
            .tracks
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(paged, full);
    }

    #[tokio::test]
    async fn tracks_page_past_end_should_be_empty() {
        let page = store().await.tracks_page(query(), 50, 10).await.unwrap();
        assert!(page.items.is_empty() && page.total == 10);
    }

    #[tokio::test]
    async fn collection_tracks_page_should_follow_collection_order() {
        let q = LibraryQuery {
            collection: Some("p.1".to_owned()),
            ..query()
        };
        let page = store().await.tracks_page(q, 2, 3).await.unwrap();
        assert_eq!(ids(&page), ["i.7", "i.6", "i.5"]);
    }

    #[tokio::test]
    async fn search_should_filter_in_sql_accent_insensitively() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![
                    song("1", "Český sen", "A", "X"),
                    song("2", "Other", "B", "Y"),
                ],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let q = LibraryQuery {
            filter: LibraryFilter::All,
            search: "cesky".to_owned(),
            ..LibraryQuery::default()
        };
        let page = storage.tracks_page(q, 0, 10).await.unwrap();
        assert_eq!((ids(&page), page.total), (vec!["1"], 1));
    }

    #[tokio::test]
    async fn missing_audio_filter_should_count_only_tracks_without_audio() {
        let storage = store().await;
        storage
            .set_track_audio(
                "i.3",
                crate::storage::AudioVariant::Stereo,
                Some(PathBuf::from("a.m4a")),
            )
            .await
            .unwrap();
        let q = LibraryQuery {
            tracks: TrackFilter::MissingAudio,
            ..query()
        };
        let page = storage.tracks_page(q, 0, 100).await.unwrap();
        assert_eq!(page.total, 9);
    }

    #[tokio::test]
    async fn every_lyrics_source_should_round_trip_through_the_database() {
        use crate::storage::LyricsSource;
        let storage = store().await;
        for source in [
            LyricsSource::Lrclib,
            LyricsSource::Amll,
            LyricsSource::Netease,
            LyricsSource::Kugou,
            LyricsSource::Musixmatch,
            LyricsSource::Community,
        ] {
            storage
                .set_track_lyrics_source("i.9", source)
                .await
                .unwrap();
            let track = storage.get_track("i.9").await.unwrap().unwrap();
            assert_eq!(track.lyrics_source, source);
        }
    }

    #[tokio::test]
    async fn missing_lyrics_filter_should_apply_inside_collection() {
        let storage = store().await;
        storage
            .set_track_lyrics_source("i.9", crate::storage::LyricsSource::Lrclib)
            .await
            .unwrap();
        let q = LibraryQuery {
            tracks: TrackFilter::MissingLyrics,
            collection: Some("p.1".to_owned()),
            ..query()
        };
        let page = storage.tracks_page(q, 0, 1).await.unwrap();
        assert_eq!((ids(&page), page.total), (vec!["i.8"], 9));
    }

    #[tokio::test]
    async fn tracks_page_should_flag_mapped_tracks() {
        let storage = store().await;
        storage
            .set_track_mapping(crate::storage::TrackMapping {
                track_id: "i.0".to_owned(),
                file_path: PathBuf::from("x.mp3"),
            })
            .await
            .unwrap();
        let page = storage.tracks_page(query(), 0, 1).await.unwrap();
        assert!(page.items[0].has_mapping);
    }

    async fn many_collections() -> Storage {
        let storage = Storage::open_in_memory().unwrap();
        let collections = (0..7)
            .map(|n| SyncedCollection {
                collection: Playlist::minimal(
                    format!("p.{n}"),
                    CollectionKind::Playlist,
                    format!("List {n}"),
                ),
                track_ids: Vec::new(),
                reported_total: Some(0),
            })
            .collect();
        storage
            .replace_library(LibrarySnapshot {
                collections,
                tracks: Vec::new(),
                songs: Vec::new(),
            })
            .await
            .unwrap();
        storage
    }

    #[tokio::test]
    async fn collection_pages_should_match_full_view_order() {
        let storage = many_collections().await;
        let mut paged = Vec::new();
        for offset in (0..7).step_by(2) {
            let page = storage.collections_page(query(), offset, 2).await.unwrap();
            assert_eq!(page.total, 7);
            paged.extend(page.items.into_iter().map(|c| c.collection.id));
        }
        let full: Vec<String> = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap()
            .collections
            .into_iter()
            .map(|c| c.collection.id)
            .collect();
        assert_eq!(paged, full);
    }

    #[tokio::test]
    async fn collection_search_should_count_matches_only() {
        let q = LibraryQuery {
            search: "list 3".to_owned(),
            ..query()
        };
        let page = many_collections()
            .await
            .collections_page(q, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.total, 1);
    }

    #[tokio::test]
    async fn cached_view_should_refresh_after_a_write() {
        let storage = many_collections().await;
        assert_eq!(
            storage.collections_page(query(), 0, 1).await.unwrap().total,
            7
        );
        storage
            .replace_library(LibrarySnapshot::default())
            .await
            .unwrap();
        assert_eq!(
            storage.collections_page(query(), 0, 1).await.unwrap().total,
            0
        );
    }

    #[tokio::test]
    async fn collection_members_should_resolve_synced_order() {
        let members = store()
            .await
            .collection_members(LibraryFilter::AppleMusic, "p.1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(members.first().map(String::as_str), Some("i.9"));
    }

    /// `p.1` of [`store`] with songs `i.0` … `i.{with_audio - 1}` downloaded
    /// (stereo). `p.1` pins the first `linked` of those, another playlist
    /// (`p.other`) the rest.
    async fn mix_with(with_audio: usize, linked: usize) -> Storage {
        let storage = store().await;
        for n in 0..with_audio {
            storage
                .set_track_audio(
                    &format!("i.{n}"),
                    crate::storage::AudioVariant::Stereo,
                    Some(PathBuf::from(format!("media/{n}.m4a"))),
                )
                .await
                .unwrap();
            let owner = if n < linked { "p.1" } else { "p.other" };
            storage
                .pin_downloads(Some(owner), vec![format!("i.{n}")])
                .await
                .unwrap();
        }
        storage
    }

    /// The Stereo summary of `p.1` in `storage`.
    async fn mix_summary(storage: &Storage) -> CollectionDownload {
        let mut found = storage
            .collection_downloads(
                LibraryFilter::AppleMusic,
                vec!["p.1".to_owned()],
                DownloadFormat::Stereo,
            )
            .await
            .unwrap();
        found.remove("p.1").unwrap()
    }

    /// [`mix_with`] with every downloaded song pinned by `p.1`, summarized.
    async fn mix_with_audio(with_audio: usize) -> CollectionDownload {
        mix_summary(&mix_with(with_audio, with_audio).await).await
    }

    /// A [`super::super::DownloadCollection`] for `owner` in the Apple Music
    /// view.
    fn playlist(owner: &str) -> crate::storage::DownloadCollection {
        crate::storage::DownloadCollection {
            owner: owner.to_owned(),
            filter: LibraryFilter::AppleMusic,
            kind: crate::storage::DownloadSourceKind::Playlist,
            name: "Mix".to_owned(),
            keep: false,
        }
    }

    fn mix_keys() -> Vec<String> {
        (0..10).map(|n| format!("i.{n}")).collect()
    }

    #[tokio::test]
    async fn collection_downloads_should_count_songs_with_files() {
        let summary = mix_with_audio(3).await;
        assert_eq!(
            (summary.state(), summary.complete, summary.songs),
            (crate::CollectionDownloadState::Partial, 3, 10)
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_report_a_collection_without_files() {
        assert_eq!(
            mix_with_audio(0).await.state(),
            crate::CollectionDownloadState::NotDownloaded
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_report_a_complete_collection() {
        assert_eq!(
            mix_with_audio(10).await.state(),
            crate::CollectionDownloadState::Downloaded
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_report_files_saved_through_other_downloads() {
        let summary = mix_summary(&mix_with(10, 0).await).await;
        assert_eq!(
            (summary.state(), summary.complete, summary.linked),
            (crate::CollectionDownloadState::SavedElsewhere, 10, 0)
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_report_a_partly_linked_complete_collection_as_saved_elsewhere()
     {
        let summary = mix_summary(&mix_with(10, 4).await).await;
        assert_eq!(
            (summary.state(), summary.linked),
            (crate::CollectionDownloadState::SavedElsewhere, 4)
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_count_a_partial_collection_from_files_whoever_pins_it() {
        let summary = mix_summary(&mix_with(3, 0).await).await;
        assert_eq!(
            (summary.state(), summary.complete),
            (crate::CollectionDownloadState::Partial, 3)
        );
    }

    #[tokio::test]
    async fn linking_should_make_a_collection_saved_elsewhere_downloaded() {
        let storage = mix_with(10, 0).await;
        storage
            .link_download_collection(playlist("p.1"), None, mix_keys())
            .await
            .unwrap();
        assert_eq!(
            mix_summary(&storage).await.state(),
            crate::CollectionDownloadState::Downloaded
        );
    }

    #[tokio::test]
    async fn linking_should_pin_every_song_and_report_the_new_pins() {
        let storage = mix_with(10, 4).await;
        let added = storage
            .link_download_collection(playlist("p.1"), None, mix_keys())
            .await
            .unwrap();
        let mut owners = Vec::new();
        for key in mix_keys() {
            owners.push(storage.download_owners(&key).await.unwrap());
        }
        assert_eq!(
            (
                added,
                owners
                    .iter()
                    .filter(|o| o.contains(&"p.1".to_owned()))
                    .count()
            ),
            (6, 10)
        );
    }

    #[tokio::test]
    async fn linking_should_leave_other_pins_alone() {
        let storage = mix_with(10, 4).await;
        storage
            .pin_downloads(None, vec!["i.9".to_owned()])
            .await
            .unwrap();
        storage
            .link_download_collection(playlist("p.1"), None, mix_keys())
            .await
            .unwrap();
        assert_eq!(
            storage.download_owners("i.9").await.unwrap(),
            ["", "p.1", "p.other"]
        );
    }

    #[tokio::test]
    async fn linking_should_register_the_collection() {
        let storage = mix_with(10, 0).await;
        storage
            .link_download_collection(playlist("p.1"), Some(true), mix_keys())
            .await
            .unwrap();
        assert_eq!(
            storage.download_collections().await.unwrap(),
            [crate::storage::DownloadCollection {
                keep: true,
                ..playlist("p.1")
            }]
        );
    }

    #[tokio::test]
    async fn linking_should_change_no_file() {
        let storage = mix_with(10, 0).await;
        let before = storage.get_track("i.3").await.unwrap().unwrap().audio;
        storage
            .link_download_collection(playlist("p.1"), None, mix_keys())
            .await
            .unwrap();
        assert_eq!(
            storage.get_track("i.3").await.unwrap().unwrap().audio,
            before
        );
    }

    #[tokio::test]
    async fn linking_through_another_id_of_a_song_should_count_as_linked() {
        // `p.1` lists the song as `a.5`; its pin is keyed on the song id `5`.
        let storage = twin_library().await;
        storage
            .set_track_audio(
                "5",
                crate::storage::AudioVariant::Stereo,
                Some(PathBuf::from("media/5.m4a")),
            )
            .await
            .unwrap();
        storage
            .link_download_collection(playlist("p.1"), None, vec!["a.5".to_owned()])
            .await
            .unwrap();
        let state = storage
            .collection_downloads(
                LibraryFilter::AppleMusic,
                vec!["p.1".to_owned(), "l.1".to_owned()],
                DownloadFormat::Stereo,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|(id, summary)| (id, summary.state()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            state.into_iter().collect::<Vec<_>>(),
            [
                (
                    "l.1".to_owned(),
                    crate::CollectionDownloadState::SavedElsewhere
                ),
                ("p.1".to_owned(), crate::CollectionDownloadState::Downloaded),
            ]
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_ignore_pins_and_finished_queue_rows() {
        // The owner's "Mix 2026": every song pinned by the playlist and its
        // queue rows "done", but no file on disk (cancelled or removed).
        let storage = store().await;
        let keys: Vec<String> = (0..10).map(|n| format!("i.{n}")).collect();
        storage
            .pin_downloads(Some("p.1"), keys.clone())
            .await
            .unwrap();
        for key in &keys {
            storage
                .upsert_download(crate::storage::DownloadRecord {
                    id: key.clone(),
                    track_id: key.clone(),
                    title: "Song".to_owned(),
                    artist: "Artist".to_owned(),
                    album: None,
                    duration_ms: 0,
                    artwork_url: None,
                    source_kind: crate::storage::DownloadSourceKind::Playlist,
                    source_id: Some("p.1".to_owned()),
                    source_name: Some("Mix".to_owned()),
                    status: crate::storage::DownloadStatus::Done,
                    progress: 1.0,
                    message: None,
                    deferred: false,
                    replace: false,
                    created_at: 0,
                    updated_at: 0,
                })
                .await
                .unwrap();
        }
        let summary = storage
            .collection_downloads(
                LibraryFilter::AppleMusic,
                vec!["p.1".to_owned()],
                DownloadFormat::Stereo,
            )
            .await
            .unwrap()
            .remove("p.1")
            .unwrap();
        let downloaded = storage
            .tracks_page(
                LibraryQuery {
                    filter: LibraryFilter::Downloaded,
                    ..LibraryQuery::default()
                },
                0,
                100,
            )
            .await
            .unwrap()
            .total;
        assert_eq!(
            (summary.state(), downloaded),
            (crate::CollectionDownloadState::NotDownloaded, 0)
        );
    }

    #[tokio::test]
    async fn collection_downloads_should_leave_out_unknown_collections() {
        let found = store()
            .await
            .collection_downloads(
                LibraryFilter::AppleMusic,
                vec!["p.nope".to_owned()],
                DownloadFormat::Stereo,
            )
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    /// The owner's "Mix 2026" on a copy of their database (`CIDER_DB_COPY`,
    /// collection `CIDER_COLLECTION`): its state under each format as
    /// stored, then after its files are cleared down to three songs (a
    /// cancelled download: pins and "done" queue rows stay), then with no
    /// file at all. Modifies the copy.
    #[tokio::test]
    #[ignore = "needs a copy of a real database in CIDER_DB_COPY"]
    #[expect(
        clippy::print_stdout,
        reason = "a manual check that reports what it saw"
    )]
    async fn real_db_copy_collection_downloads() {
        let path = PathBuf::from(std::env::var_os("CIDER_DB_COPY").expect("CIDER_DB_COPY"));
        let owner = std::env::var("CIDER_COLLECTION").expect("CIDER_COLLECTION");
        let storage = Storage::open_async(path).await.unwrap();
        let filter = LibraryFilter::AppleMusic;
        let report = |label: &'static str| {
            let storage = storage.clone();
            let owner = owner.clone();
            async move {
                for format in [
                    DownloadFormat::Stereo,
                    DownloadFormat::SpatialWhereAvailable,
                    DownloadFormat::Both,
                ] {
                    let summary = storage
                        .collection_downloads(filter, vec![owner.clone()], format)
                        .await
                        .unwrap()
                        .remove(&owner)
                        .unwrap();
                    println!(
                        "{label}: {format:?}: {:?}, {} of {} complete, {} with audio",
                        summary.state(),
                        summary.complete,
                        summary.songs,
                        summary.with_audio
                    );
                }
                let pinned = storage.pinned_collections().await.unwrap().contains(&owner);
                println!("{label}: pinned {pinned}");
            }
        };
        report("as stored").await;
        let members = storage
            .collection_members(filter, &owner)
            .await
            .unwrap()
            .unwrap_or_default();
        for member in members.iter().skip(3) {
            storage.clear_audio_by_any_id(member).await.unwrap();
        }
        report("three songs left").await;
        for member in members.iter().take(3) {
            storage.clear_audio_by_any_id(member).await.unwrap();
        }
        report("no files").await;
        let downloaded = storage
            .tracks_page(
                LibraryQuery {
                    filter: LibraryFilter::Downloaded,
                    ..LibraryQuery::default()
                },
                0,
                0,
            )
            .await
            .unwrap()
            .total;
        println!("no files: Downloaded songs list {downloaded}");
    }
}
