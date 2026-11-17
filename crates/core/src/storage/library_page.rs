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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, ToSql};

use super::library::{self, LibraryCollection, LibraryFilter, sort_fold};
use super::models::{CollectionKind, Track};
use super::paths::PathCodec;
use super::repo::{TRACK_COLUMNS, track_from_row};
use super::{Storage, StorageError};

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

fn track_filter_predicate(filter: TrackFilter) -> &'static str {
    match filter {
        TrackFilter::All => "1",
        TrackFilter::MissingAudio => "t.audio_path IS NULL",
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
    let columns = TRACK_COLUMNS
        .split(", ")
        .map(|column| format!("t.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mapping_column = TRACK_COLUMNS.split(", ").count();
    let mut stmt = conn.prepare(&format!(
        "SELECT {columns},
                EXISTS (SELECT 1 FROM track_mappings m WHERE m.track_id = t.id)
         FROM {from} WHERE {condition}
         ORDER BY {order} LIMIT :limit OFFSET :offset"
    ))?;
    let items = stmt
        .query_map(params.as_slice(), |row| {
            Ok(LibraryTrack {
                track: track_from_row(codec)(row)?,
                has_mapping: row.get(mapping_column)?,
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
            .set_track_audio("i.3", Some(PathBuf::from("a.m4a")))
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
}
