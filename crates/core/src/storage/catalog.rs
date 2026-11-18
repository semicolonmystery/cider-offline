//! Repository functions for the offline features: catalog metadata merged
//! from observed API responses, settings, and the artwork file index.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::models::{LyricsSource, Playlist, Track};
use super::paths::PathCodec;
use super::repo::{TRACK_COLUMNS, now_unix, track_from_row, upsert_playlist};
use super::{Storage, StorageError};

/// Track metadata observed in an Apple Music API response.
///
/// Merging it never touches the track's local audio file or lyrics state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMetadata {
    /// Apple Music id (catalog id or library `i.…` id).
    pub id: String,
    /// Catalog id of a library track (`playParams.catalogId`).
    pub catalog_id: Option<String>,
    /// Title.
    pub title: String,
    /// Artist name.
    pub artist: String,
    /// Album name.
    pub album: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u32>,
    /// Artwork URL template.
    pub artwork_url: Option<String>,
    /// Raw JSON resource.
    pub metadata: Option<String>,
    /// Album artist name (`albumArtistName`).
    pub album_artist: Option<String>,
    /// Composer name (`composerName`).
    pub composer: Option<String>,
    /// Primary genre (first of `genreNames`).
    pub genre: Option<String>,
    /// Track number within its album.
    pub track_number: Option<u32>,
    /// Disc number.
    pub disc_number: Option<u32>,
    /// Release date (ISO string).
    pub release_date: Option<String>,
    /// ISRC.
    pub isrc: Option<String>,
    /// Canonical music.apple.com URL.
    pub url: Option<String>,
    /// Whether lyrics are available (`hasLyrics`).
    pub has_lyrics: Option<bool>,
    /// Whether time-synced lyrics are available (`hasTimeSyncedLyrics`).
    pub has_time_synced_lyrics: Option<bool>,
    /// Content rating (e.g. `"explicit"`).
    pub content_rating: Option<String>,
    /// Whether this observation proves the song is a saved member of the user's
    /// Apple Music library (top-level `data[]` of `/v1/me/library/songs`, or a
    /// saved library album's tracks). `false` for songs seen only inside a
    /// playlist, in catalog responses or in search. Merged with MAX so it is
    /// sticky once set.
    pub in_library: bool,
}

/// The ordered track list of a playlist or album, as observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackListUpdate {
    /// Playlist / album id.
    pub playlist_id: String,
    /// Track ids in order, starting at `offset`.
    pub track_ids: Vec<String>,
    /// Position of the first id (pagination offset).
    pub offset: usize,
    /// Whether the list is known to be complete from `offset` on (no `next` page).
    pub complete: bool,
}

/// Everything extracted from one API response, applied in one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogUpdate {
    /// Tracks to merge.
    pub tracks: Vec<TrackMetadata>,
    /// Playlists / albums to upsert.
    pub collections: Vec<Playlist>,
    /// Track lists to apply (only for collections that exist).
    pub track_lists: Vec<TrackListUpdate>,
}

impl CatalogUpdate {
    /// Whether there is nothing to apply.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.collections.is_empty() && self.track_lists.is_empty()
    }
}

/// A track row together with its catalog id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRecord {
    /// The track.
    pub track: Track,
    /// Catalog id of a library track, if known.
    pub catalog_id: Option<String>,
}

impl TrackRecord {
    /// The id lyrics and catalog requests use for this track: the catalog id
    /// when known, otherwise the track id.
    pub fn catalog_or_id(&self) -> &str {
        self.catalog_id.as_deref().unwrap_or(&self.track.id)
    }
}

/// An artwork image stored as a file under the artwork directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkFile {
    /// Normalized cache key of the image URL.
    pub cache_key: String,
    /// Image path without the size segment (see [`crate::artwork::base_key`]).
    pub base_key: String,
    /// File name inside the artwork directory.
    pub file_name: String,
    /// `Content-Type` of the image.
    pub content_type: String,
    /// Size in bytes.
    pub size: u64,
    /// `width * height` parsed from the URL, `0` if unknown.
    pub pixels: u64,
}

// ----- Catalog ----------------------------------------------------------------

pub(super) fn merge_track(conn: &Connection, track: &TrackMetadata) -> Result<(), StorageError> {
    // Rich columns are filled when the observed resource carries them (Detail =
    // Full); each is preserved when a later, leaner observation omits it. The
    // NOT NULL boolean flags reference their parameter directly (not `excluded`,
    // which the INSERT coalesces to 0) so a `None` keeps the stored value.
    conn.execute(
        "INSERT INTO tracks
           (id, catalog_id, title, artist, album, duration_ms, artwork_url, metadata,
            album_artist, composer, genre, track_number, disc_number, release_date, isrc, url,
            has_lyrics, has_time_synced_lyrics, content_rating, in_library, lyrics_source, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 COALESCE(?17, 0), COALESCE(?18, 0), ?19, ?20, 'none', ?21)
         ON CONFLICT(id) DO UPDATE SET
           catalog_id = COALESCE(excluded.catalog_id, tracks.catalog_id),
           title = excluded.title, artist = excluded.artist,
           album = COALESCE(excluded.album, tracks.album),
           duration_ms = COALESCE(excluded.duration_ms, tracks.duration_ms),
           artwork_url = COALESCE(excluded.artwork_url, tracks.artwork_url),
           metadata = COALESCE(excluded.metadata, tracks.metadata),
           album_artist = COALESCE(excluded.album_artist, tracks.album_artist),
           composer = COALESCE(excluded.composer, tracks.composer),
           genre = COALESCE(excluded.genre, tracks.genre),
           track_number = COALESCE(excluded.track_number, tracks.track_number),
           disc_number = COALESCE(excluded.disc_number, tracks.disc_number),
           release_date = COALESCE(excluded.release_date, tracks.release_date),
           isrc = COALESCE(excluded.isrc, tracks.isrc),
           url = COALESCE(excluded.url, tracks.url),
           has_lyrics = COALESCE(?17, tracks.has_lyrics),
           has_time_synced_lyrics = COALESCE(?18, tracks.has_time_synced_lyrics),
           content_rating = COALESCE(excluded.content_rating, tracks.content_rating),
           -- Sticky: a later playlist capture of the same song (in_library = 0)
           -- never clears a previously-proven library membership.
           in_library = MAX(tracks.in_library, excluded.in_library),
           updated_at = excluded.updated_at",
        params![
            track.id,
            track.catalog_id,
            track.title,
            track.artist,
            track.album,
            track.duration_ms,
            track.artwork_url,
            track.metadata,
            track.album_artist,
            track.composer,
            track.genre,
            track.track_number,
            track.disc_number,
            track.release_date,
            track.isrc,
            track.url,
            track.has_lyrics,
            track.has_time_synced_lyrics,
            track.content_rating,
            track.in_library,
            now_unix()
        ],
    )?;
    Ok(())
}

fn track_ids_of(conn: &Connection, playlist_id: &str) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn
        .prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position")?;
    let rows = stmt.query_map([playlist_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Computes the new track list from the stored one and an observed page.
pub(crate) fn merged_track_list(existing: &[String], update: &TrackListUpdate) -> Vec<String> {
    let keep = update.offset.min(existing.len());
    let mut merged: Vec<String> = existing[..keep].to_vec();
    merged.extend(update.track_ids.iter().cloned());
    if !update.complete {
        let tail_start = merged.len();
        if existing.len() > tail_start {
            merged.extend(existing[tail_start..].iter().cloned());
        }
    }
    merged
}

fn apply_track_list(conn: &Connection, update: &TrackListUpdate) -> Result<(), StorageError> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM playlists WHERE id = ?1",
            [&update.playlist_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !exists {
        return Ok(());
    }
    let existing = track_ids_of(conn, &update.playlist_id)?;
    let merged = merged_track_list(&existing, update);
    if merged == existing {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM playlist_tracks WHERE playlist_id = ?1",
        [&update.playlist_id],
    )?;
    let mut insert = conn.prepare(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
    )?;
    for (position, track_id) in (0_i64..).zip(&merged) {
        insert.execute(params![update.playlist_id, position, track_id])?;
    }
    Ok(())
}

fn apply_catalog(conn: &mut Connection, update: &CatalogUpdate) -> Result<(), StorageError> {
    let tx = conn.transaction()?;
    for track in &update.tracks {
        merge_track(&tx, track)?;
    }
    for collection in &update.collections {
        upsert_playlist(&tx, collection)?;
    }
    for list in &update.track_lists {
        apply_track_list(&tx, list)?;
    }
    tx.commit()?;
    Ok(())
}

/// Columns for building a [`TrackRecord`]. `catalog_id` is now one of
/// [`TRACK_COLUMNS`] (carried on [`Track`] itself), so the record reuses them.
fn record_columns() -> String {
    TRACK_COLUMNS.to_owned()
}

fn record_from_row(codec: &PathCodec) -> impl Fn(&Row<'_>) -> rusqlite::Result<TrackRecord> + '_ {
    let track_from_row = track_from_row(codec);
    move |row| {
        let track = track_from_row(row)?;
        let catalog_id = track.catalog_id.clone();
        Ok(TrackRecord { track, catalog_id })
    }
}

fn find_track(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
) -> Result<Option<TrackRecord>, StorageError> {
    // Exact id first, then a library track whose catalog id matches.
    let columns = record_columns();
    Ok(conn
        .query_row(
            &format!(
                "SELECT {columns} FROM tracks WHERE id = ?1 OR catalog_id = ?1
                 ORDER BY (id = ?1) DESC, (audio_path IS NOT NULL) DESC LIMIT 1"
            ),
            [id],
            record_from_row(codec),
        )
        .optional()?)
}

/// Every track row whose id or catalog id is `id`, catalog-native rows first.
fn tracks_by_any_id(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
) -> Result<Vec<TrackRecord>, StorageError> {
    let columns = record_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {columns} FROM tracks WHERE id = ?1 OR catalog_id = ?1
         ORDER BY (id = ?1) DESC, in_library DESC, id"
    ))?;
    let rows = stmt.query_map([id], record_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Real tracks other than `exclude_id` with the same title and artist
/// (ASCII case-insensitive).
fn same_title_and_artist(
    conn: &Connection,
    codec: &PathCodec,
    title: &str,
    artist: &str,
    exclude_id: &str,
) -> Result<Vec<TrackRecord>, StorageError> {
    let columns = record_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {columns} FROM tracks
         WHERE title = ?1 COLLATE NOCASE AND artist = ?2 COLLATE NOCASE
           AND id <> ?3 AND is_custom = 0
         ORDER BY in_library DESC, id"
    ))?;
    let rows = stmt.query_map(params![title, artist, exclude_id], record_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn tracks_by_lyrics(
    conn: &Connection,
    codec: &PathCodec,
    source: LyricsSource,
) -> Result<Vec<TrackRecord>, StorageError> {
    let columns = record_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {columns} FROM tracks WHERE lyrics_source = ?1 ORDER BY artist, album, title"
    ))?;
    let rows = stmt.query_map([source.as_str()], record_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn search_tracks(
    conn: &Connection,
    codec: &PathCodec,
    term: &str,
    limit: usize,
) -> Result<Vec<TrackRecord>, StorageError> {
    let columns = record_columns();
    let pattern = format!("%{}%", escape_like(term));
    let mut stmt = conn.prepare(&format!(
        "SELECT {columns} FROM tracks
         WHERE metadata IS NOT NULL
           AND (title LIKE ?1 ESCAPE '\\' OR artist LIKE ?1 ESCAPE '\\' OR album LIKE ?1 ESCAPE '\\')
         ORDER BY title LIMIT ?2"
    ))?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![pattern, limit], record_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn set_lyrics_source_any(
    conn: &Connection,
    id: &str,
    source: LyricsSource,
) -> Result<usize, StorageError> {
    Ok(conn.execute(
        "UPDATE tracks SET lyrics_source = ?2, updated_at = ?3 WHERE id = ?1 OR catalog_id = ?1",
        params![id, source.as_str(), now_unix()],
    )?)
}

// ----- Settings ---------------------------------------------------------------

pub(super) fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now_unix()],
    )?;
    Ok(())
}

pub(super) fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

// ----- Artwork ----------------------------------------------------------------

fn artwork_from_row(row: &Row<'_>) -> rusqlite::Result<ArtworkFile> {
    Ok(ArtworkFile {
        cache_key: row.get(0)?,
        base_key: row.get(1)?,
        file_name: row.get(2)?,
        content_type: row.get(3)?,
        size: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
        pixels: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
    })
}

const ARTWORK_COLUMNS: &str = "cache_key, base_key, file_name, content_type, size, pixels";

/// Inserts or replaces an artwork row; returns the file name it replaced, if different.
fn put_artwork(conn: &Connection, file: &ArtworkFile) -> Result<Option<String>, StorageError> {
    let previous: Option<String> = conn
        .query_row(
            "SELECT file_name FROM artwork_files WHERE cache_key = ?1",
            [&file.cache_key],
            |row| row.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT INTO artwork_files (cache_key, base_key, file_name, content_type, size, pixels,
                                    stored_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(cache_key) DO UPDATE SET
           base_key = excluded.base_key, file_name = excluded.file_name,
           content_type = excluded.content_type, size = excluded.size,
           pixels = excluded.pixels, stored_at = excluded.stored_at",
        params![
            file.cache_key,
            file.base_key,
            file.file_name,
            file.content_type,
            i64::try_from(file.size).unwrap_or(i64::MAX),
            i64::try_from(file.pixels).unwrap_or(i64::MAX),
            now_unix()
        ],
    )?;
    Ok(previous.filter(|name| *name != file.file_name))
}

fn get_artwork(conn: &Connection, cache_key: &str) -> Result<Option<ArtworkFile>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {ARTWORK_COLUMNS} FROM artwork_files WHERE cache_key = ?1"),
            [cache_key],
            artwork_from_row,
        )
        .optional()?)
}

fn best_artwork(conn: &Connection, base_key: &str) -> Result<Option<ArtworkFile>, StorageError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {ARTWORK_COLUMNS} FROM artwork_files WHERE base_key = ?1
                 ORDER BY pixels DESC, stored_at DESC LIMIT 1"
            ),
            [base_key],
            artwork_from_row,
        )
        .optional()?)
}

/// The smallest stored size of the image with at least `min_pixels` pixels,
/// else its largest stored size.
fn artwork_at_least(
    conn: &Connection,
    base_key: &str,
    min_pixels: u64,
) -> Result<Option<ArtworkFile>, StorageError> {
    let min = i64::try_from(min_pixels).unwrap_or(i64::MAX);
    Ok(conn
        .query_row(
            &format!(
                "SELECT {ARTWORK_COLUMNS} FROM artwork_files WHERE base_key = ?1
                 ORDER BY pixels >= ?2 DESC,
                          CASE WHEN pixels >= ?2 THEN pixels ELSE -pixels END ASC,
                          stored_at DESC
                 LIMIT 1"
            ),
            params![base_key, min],
            artwork_from_row,
        )
        .optional()?)
}

fn clear_artwork(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let names = {
        let mut stmt = conn.prepare("SELECT file_name FROM artwork_files")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()?
    };
    conn.execute("DELETE FROM artwork_files", [])?;
    Ok(names)
}

impl Storage {
    /// Merges catalog metadata observed in an API response (tracks,
    /// playlists/albums, track lists) in one transaction. Existing local
    /// audio paths and lyrics state are preserved.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn apply_catalog_update(&self, update: CatalogUpdate) -> Result<(), StorageError> {
        self.call(move |c| apply_catalog(c, &update)).await
    }

    /// Finds a track by id, or a library track whose catalog id is `id`.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn find_track(&self, id: &str) -> Result<Option<TrackRecord>, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| find_track(c, &codec, &id)).await
    }

    /// Every track row whose id or catalog id is `id` (the row with that id
    /// first, then saved-library rows).
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn tracks_by_any_id(&self, id: &str) -> Result<Vec<TrackRecord>, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| tracks_by_any_id(c, &codec, &id)).await
    }

    /// Real (non-custom) tracks other than `exclude_id` whose title and artist
    /// equal the given ones, ignoring ASCII case. Used to find the playable
    /// twin of a library-only item and the album version of a music video.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn tracks_with_title_and_artist(
        &self,
        title: &str,
        artist: &str,
        exclude_id: &str,
    ) -> Result<Vec<TrackRecord>, StorageError> {
        let (title, artist, exclude_id) =
            (title.to_owned(), artist.to_owned(), exclude_id.to_owned());
        let codec = self.codec.clone();
        self.call(move |c| same_title_and_artist(c, &codec, &title, &artist, &exclude_id))
            .await
    }

    /// Records that the library item `library_id`, which has no
    /// `playParams.catalogId` of its own, plays as catalog id `catalog_id`
    /// (its resolved twin). Only fills an empty link; returns whether the row
    /// changed. Afterwards lookups, audio and lyrics by either id reach the
    /// row, so it shows as downloaded once the twin is.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn link_catalog_twin(
        &self,
        library_id: &str,
        catalog_id: &str,
    ) -> Result<bool, StorageError> {
        let (library_id, catalog_id) = (library_id.to_owned(), catalog_id.to_owned());
        self.call(move |c| {
            Ok(c.execute(
                "UPDATE tracks SET catalog_id = ?2, updated_at = ?3
                 WHERE id = ?1 AND catalog_id IS NULL AND is_custom = 0",
                params![library_id, catalog_id, now_unix()],
            )? > 0)
        })
        .await
    }

    /// Tracks with the given lyrics state, ordered by artist, album, title.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn tracks_with_lyrics_source(
        &self,
        source: LyricsSource,
    ) -> Result<Vec<TrackRecord>, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| tracks_by_lyrics(c, &codec, source))
            .await
    }

    /// Tracks with stored metadata whose title, artist or album contains `term`.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn search_tracks(
        &self,
        term: &str,
        limit: usize,
    ) -> Result<Vec<TrackRecord>, StorageError> {
        let term = term.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| search_tracks(c, &codec, &term, limit))
            .await
    }

    /// Sets the lyrics source of every track whose id or catalog id is `id`;
    /// returns how many rows changed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_lyrics_source_by_any_id(
        &self,
        id: &str,
        source: LyricsSource,
    ) -> Result<usize, StorageError> {
        let id = id.to_owned();
        self.call(move |c| set_lyrics_source_any(c, &id, source))
            .await
    }

    /// Stores a setting.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_setting(&self, key: &str, value: String) -> Result<(), StorageError> {
        let key = key.to_owned();
        self.call(move |c| set_setting(c, &key, &value)).await
    }

    /// Reads a setting.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>, StorageError> {
        let key = key.to_owned();
        self.call(move |c| get_setting(c, &key)).await
    }

    /// Records an artwork file; returns the replaced file name (to delete), if any.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn put_artwork(&self, file: ArtworkFile) -> Result<Option<String>, StorageError> {
        self.call(move |c| put_artwork(c, &file)).await
    }

    /// Artwork stored for an exact cache key.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_artwork(&self, cache_key: &str) -> Result<Option<ArtworkFile>, StorageError> {
        let key = cache_key.to_owned();
        self.call(move |c| get_artwork(c, &key)).await
    }

    /// Largest stored size of the image with this base key.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn best_artwork(&self, base_key: &str) -> Result<Option<ArtworkFile>, StorageError> {
        let key = base_key.to_owned();
        self.call(move |c| best_artwork(c, &key)).await
    }

    /// The smallest stored size of the image with this base key that has at
    /// least `min_pixels` pixels, falling back to its largest stored size.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn artwork_at_least(
        &self,
        base_key: &str,
        min_pixels: u64,
    ) -> Result<Option<ArtworkFile>, StorageError> {
        let key = base_key.to_owned();
        self.call(move |c| artwork_at_least(c, &key, min_pixels))
            .await
    }

    /// Removes every artwork row; returns the file names to delete.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn clear_artwork_index(&self) -> Result<Vec<String>, StorageError> {
        self.call(|c| clear_artwork(c)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::CollectionKind;
    use std::path::PathBuf;

    fn meta(id: &str) -> TrackMetadata {
        TrackMetadata {
            id: id.to_owned(),
            catalog_id: None,
            title: "T".to_owned(),
            artist: "A".to_owned(),
            album: None,
            duration_ms: Some(1000),
            artwork_url: None,
            metadata: Some("{}".to_owned()),
            album_artist: None,
            composer: None,
            genre: None,
            track_number: None,
            disc_number: None,
            release_date: None,
            isrc: None,
            url: None,
            has_lyrics: None,
            has_time_synced_lyrics: None,
            content_rating: None,
            in_library: false,
        }
    }

    fn list(ids: &[&str], offset: usize, complete: bool) -> TrackListUpdate {
        TrackListUpdate {
            playlist_id: "p".to_owned(),
            track_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            offset,
            complete,
        }
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn merge_should_preserve_audio_path() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![meta("1")],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        storage
            .set_track_audio("1", Some(PathBuf::from("a.m4a")))
            .await
            .unwrap();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![meta("1")],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let track = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!(track.audio_path, Some(PathBuf::from("a.m4a")));
    }

    #[tokio::test]
    async fn find_track_should_match_catalog_id() {
        let storage = Storage::open_in_memory().unwrap();
        let mut track = meta("i.abc");
        track.catalog_id = Some("42".to_owned());
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![track],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let found = storage.find_track("42").await.unwrap().unwrap();
        assert_eq!(found.track.id, "i.abc");
    }

    #[tokio::test]
    async fn track_list_should_be_ignored_for_unknown_playlist() {
        let storage = Storage::open_in_memory().unwrap();
        let update = CatalogUpdate {
            track_lists: vec![list(&["1"], 0, true)],
            ..CatalogUpdate::default()
        };
        storage.apply_catalog_update(update).await.unwrap();
        assert_eq!(
            storage.playlist_track_ids("p").await.unwrap(),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn track_list_should_apply_to_known_playlist() {
        let storage = Storage::open_in_memory().unwrap();
        let update = CatalogUpdate {
            collections: vec![Playlist::minimal("p", CollectionKind::Playlist, "P")],
            track_lists: vec![list(&["1", "2"], 0, true)],
            ..CatalogUpdate::default()
        };
        storage.apply_catalog_update(update).await.unwrap();
        assert_eq!(
            storage.playlist_track_ids("p").await.unwrap(),
            strings(&["1", "2"])
        );
    }

    #[test]
    fn second_page_should_append() {
        assert_eq!(
            merged_track_list(&strings(&["1", "2"]), &list(&["3"], 2, true)),
            strings(&["1", "2", "3"])
        );
    }

    #[test]
    fn incomplete_first_page_should_keep_tail() {
        assert_eq!(
            merged_track_list(&strings(&["1", "2", "3"]), &list(&["9"], 0, false)),
            strings(&["9", "2", "3"])
        );
    }

    #[test]
    fn complete_first_page_should_replace() {
        assert_eq!(
            merged_track_list(&strings(&["1", "2", "3"]), &list(&["9"], 0, true)),
            strings(&["9"])
        );
    }

    #[tokio::test]
    async fn setting_should_roundtrip() {
        let storage = Storage::open_in_memory().unwrap();
        storage.set_setting("k", "v".to_owned()).await.unwrap();
        assert_eq!(
            storage.get_setting("k").await.unwrap().as_deref(),
            Some("v")
        );
    }

    #[tokio::test]
    async fn best_artwork_should_prefer_largest() {
        let storage = Storage::open_in_memory().unwrap();
        for (key, pixels) in [("a", 100), ("b", 900), ("c", 400)] {
            storage
                .put_artwork(ArtworkFile {
                    cache_key: key.to_owned(),
                    base_key: "base".to_owned(),
                    file_name: format!("{key}.jpg"),
                    content_type: "image/jpeg".to_owned(),
                    size: 1,
                    pixels,
                })
                .await
                .unwrap();
        }
        let best = storage.best_artwork("base").await.unwrap().unwrap();
        assert_eq!(best.cache_key, "b");
    }

    async fn sized_store() -> Storage {
        let storage = Storage::open_in_memory().unwrap();
        for (key, pixels) in [("small", 100), ("mid", 400), ("big", 900)] {
            storage
                .put_artwork(ArtworkFile {
                    cache_key: key.to_owned(),
                    base_key: "base".to_owned(),
                    file_name: format!("{key}.jpg"),
                    content_type: "image/jpeg".to_owned(),
                    size: 1,
                    pixels,
                })
                .await
                .unwrap();
        }
        storage
    }

    #[tokio::test]
    async fn artwork_at_least_should_pick_smallest_sufficient_size() {
        let storage = sized_store().await;
        let found = storage
            .artwork_at_least("base", 300)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.cache_key, "mid");
    }

    #[tokio::test]
    async fn artwork_at_least_should_fall_back_to_largest() {
        let storage = sized_store().await;
        let found = storage
            .artwork_at_least("base", 5000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.cache_key, "big");
    }

    #[tokio::test]
    async fn search_should_escape_like_wildcards() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![meta("1")],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        assert_eq!(
            storage.search_tracks("%", 10).await.unwrap(),
            Vec::<TrackRecord>::new()
        );
    }
}
