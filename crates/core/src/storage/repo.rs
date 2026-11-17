//! Synchronous repository functions. Called on a blocking thread by
//! [`super::Storage`]; never call these from async code directly.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::StorageError;
use super::models::{
    CUSTOM_SONG_ID_PREFIX, CacheEntry, CollectionKind, CustomAudio, CustomAudioMode, LyricsQuality,
    LyricsSource, NewCustomCollection, NewCustomTrack, Playlist, TokenKind, Track, TrackMapping,
};
use super::paths::PathCodec;

pub(super) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or_default()
}

fn path_str(path: &Path) -> Result<&str, StorageError> {
    path.to_str()
        .ok_or_else(|| StorageError::NonUtf8Path(path.to_path_buf()))
}

fn conversion_error(column: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, message.into())
}

// ----- Id sequence allocator --------------------------------------------------

/// Allocates the next value of the named monotonic counter. Values are never
/// reused, even across deletes, so a recycled `local-<n>` id can never collide
/// with one Cider may still have cached.
pub(super) fn next_seq(conn: &Connection, name: &str) -> Result<i64, StorageError> {
    conn.execute(
        "INSERT INTO id_seq (name, next) VALUES (?1, 1)
         ON CONFLICT(name) DO UPDATE SET next = id_seq.next + 1",
        [name],
    )?;
    Ok(
        conn.query_row("SELECT next FROM id_seq WHERE name = ?1", [name], |row| {
            row.get(0)
        })?,
    )
}

// ----- HTTP cache -------------------------------------------------------------

pub(super) fn put_cache_entry(conn: &Connection, entry: &CacheEntry) -> Result<(), StorageError> {
    let headers = serde_json::to_string(&entry.headers)?;
    conn.execute(
        "INSERT INTO http_cache (cache_key, method, url, status, headers, body, stored_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(cache_key) DO UPDATE SET
           method = excluded.method, url = excluded.url, status = excluded.status,
           headers = excluded.headers, body = excluded.body, stored_at = excluded.stored_at",
        params![
            entry.key,
            entry.method,
            entry.url,
            entry.status,
            headers,
            entry.body,
            entry.stored_at
        ],
    )?;
    Ok(())
}

pub(super) fn get_cache_entry(
    conn: &Connection,
    key: &str,
) -> Result<Option<CacheEntry>, StorageError> {
    let row = conn
        .query_row(
            "SELECT cache_key, method, url, status, headers, body, stored_at
             FROM http_cache WHERE cache_key = ?1",
            [key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    row.map(|(key, method, url, status, headers, body, stored_at)| {
        Ok(CacheEntry {
            key,
            method,
            url,
            status,
            headers: serde_json::from_str(&headers)?,
            body,
            stored_at,
        })
    })
    .transpose()
}

pub(super) fn delete_cache_entry(conn: &Connection, key: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM http_cache WHERE cache_key = ?1", [key])? > 0)
}

pub(super) fn clear_http_cache(conn: &Connection) -> Result<usize, StorageError> {
    Ok(conn.execute("DELETE FROM http_cache", [])?)
}

// ----- Tracks -----------------------------------------------------------------

/// Columns of [`Track`], in `track_from_row` order. `catalog_id` is last so the
/// first columns keep their indices.
pub(super) const TRACK_COLUMNS: &str = "id, title, artist, album, duration_ms, artwork_url, \
     metadata, album_artist, composer, genre, track_number, disc_number, release_date, isrc, url, \
     has_lyrics, has_time_synced_lyrics, content_rating, is_custom, real_album_id, audio_mode, \
     reference_track_id, audio_path, lyrics_source, catalog_id, in_library, lyrics_quality";

/// Row mapper for [`TRACK_COLUMNS`], resolving `audio_path` through `codec`.
pub(super) fn track_from_row(
    codec: &PathCodec,
) -> impl Fn(&Row<'_>) -> rusqlite::Result<Track> + '_ {
    move |row| track_from_row_with(codec, row)
}

fn track_from_row_with(codec: &PathCodec, row: &Row<'_>) -> rusqlite::Result<Track> {
    let audio_mode = match row.get::<_, Option<String>>(20)? {
        Some(mode) => Some(
            CustomAudioMode::parse(&mode)
                .ok_or_else(|| conversion_error(20, format!("unknown audio mode {mode:?}")))?,
        ),
        None => None,
    };
    let lyrics: String = row.get(23)?;
    Ok(Track {
        id: row.get(0)?,
        catalog_id: row.get(24)?,
        title: row.get(1)?,
        artist: row.get(2)?,
        album: row.get(3)?,
        duration_ms: row.get(4)?,
        artwork_url: row.get(5)?,
        metadata: row.get(6)?,
        album_artist: row.get(7)?,
        composer: row.get(8)?,
        genre: row.get(9)?,
        track_number: row.get(10)?,
        disc_number: row.get(11)?,
        release_date: row.get(12)?,
        isrc: row.get(13)?,
        url: row.get(14)?,
        has_lyrics: row.get(15)?,
        has_time_synced_lyrics: row.get(16)?,
        content_rating: row.get(17)?,
        is_custom: row.get(18)?,
        real_album_id: row.get(19)?,
        audio_mode,
        reference_track_id: row.get(21)?,
        audio_path: row
            .get::<_, Option<String>>(22)?
            .map(|stored| codec.decode(&stored)),
        lyrics_source: LyricsSource::parse(&lyrics)
            .ok_or_else(|| conversion_error(23, format!("unknown lyrics source {lyrics:?}")))?,
        in_library: row.get(25)?,
        lyrics_quality: {
            let quality: String = row.get(26)?;
            LyricsQuality::parse(&quality).ok_or_else(|| {
                conversion_error(26, format!("unknown lyrics quality {quality:?}"))
            })?
        },
    })
}

pub(super) fn upsert_track(
    conn: &Connection,
    codec: &PathCodec,
    track: &Track,
) -> Result<(), StorageError> {
    let audio_path = track
        .audio_path
        .as_deref()
        .map(|path| codec.encode(path))
        .transpose()?;
    conn.execute(
        "INSERT INTO tracks
           (id, title, artist, album, duration_ms, artwork_url, metadata, album_artist, composer,
            genre, track_number, disc_number, release_date, isrc, url, has_lyrics,
            has_time_synced_lyrics, content_rating, is_custom, real_album_id, audio_mode,
            reference_track_id, audio_path, lyrics_source, catalog_id, in_library, updated_at,
            lyrics_quality)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)
         ON CONFLICT(id) DO UPDATE SET
           title = excluded.title, artist = excluded.artist, album = excluded.album,
           duration_ms = excluded.duration_ms, artwork_url = excluded.artwork_url,
           metadata = excluded.metadata, album_artist = excluded.album_artist,
           composer = excluded.composer, genre = excluded.genre,
           track_number = excluded.track_number, disc_number = excluded.disc_number,
           release_date = excluded.release_date, isrc = excluded.isrc, url = excluded.url,
           has_lyrics = excluded.has_lyrics,
           has_time_synced_lyrics = excluded.has_time_synced_lyrics,
           content_rating = excluded.content_rating, is_custom = excluded.is_custom,
           real_album_id = excluded.real_album_id, audio_mode = excluded.audio_mode,
           reference_track_id = excluded.reference_track_id, audio_path = excluded.audio_path,
           lyrics_source = excluded.lyrics_source,
           lyrics_quality = excluded.lyrics_quality,
           catalog_id = COALESCE(excluded.catalog_id, tracks.catalog_id),
           -- Sticky: a later leaner capture never clears a proven membership.
           in_library = MAX(tracks.in_library, excluded.in_library),
           updated_at = excluded.updated_at",
        params![
            track.id,
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
            track.is_custom,
            track.real_album_id,
            track.audio_mode.map(CustomAudioMode::as_str),
            track.reference_track_id,
            audio_path,
            track.lyrics_source.as_str(),
            track.catalog_id,
            track.in_library,
            now_unix(),
            track.lyrics_quality.as_str(),
        ],
    )?;
    Ok(())
}

pub(super) fn get_track(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
) -> Result<Option<Track>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE id = ?1"),
            [id],
            track_from_row(codec),
        )
        .optional()?)
}

pub(super) fn list_tracks(
    conn: &Connection,
    codec: &PathCodec,
) -> Result<Vec<Track>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS} FROM tracks ORDER BY artist, album, title"
    ))?;
    let rows = stmt.query_map([], track_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn delete_track(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM tracks WHERE id = ?1", [id])? > 0)
}

pub(super) fn set_track_audio(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    audio_path: Option<&Path>,
) -> Result<bool, StorageError> {
    let audio_path = audio_path.map(|path| codec.encode(path)).transpose()?;
    Ok(conn.execute(
        "UPDATE tracks SET audio_path = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, audio_path, now_unix()],
    )? > 0)
}

pub(super) fn set_track_lyrics_source(
    conn: &Connection,
    id: &str,
    source: LyricsSource,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE tracks SET lyrics_source = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, source.as_str(), now_unix()],
    )? > 0)
}

/// Sets the lyrics source and timing of every track whose id or catalog id is
/// `id`; returns how many rows changed.
pub(super) fn set_lyrics_state_any(
    conn: &Connection,
    id: &str,
    source: LyricsSource,
    quality: LyricsQuality,
) -> Result<usize, StorageError> {
    let quality = if source == LyricsSource::None {
        LyricsQuality::None
    } else {
        quality
    };
    Ok(conn.execute(
        "UPDATE tracks SET lyrics_source = ?2, lyrics_quality = ?3, updated_at = ?4          WHERE id = ?1 OR catalog_id = ?1",
        params![id, source.as_str(), quality.as_str(), now_unix()],
    )?)
}

/// Sets `audio_path` on every track whose id or catalog id is `id`; returns
/// how many rows changed.
pub(super) fn set_audio_any(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    audio_path: Option<&Path>,
) -> Result<usize, StorageError> {
    let audio_path = audio_path.map(|path| codec.encode(path)).transpose()?;
    Ok(conn.execute(
        "UPDATE tracks SET audio_path = ?2, updated_at = ?3 WHERE id = ?1 OR catalog_id = ?1",
        params![id, audio_path, now_unix()],
    )?)
}

/// Catalog ids (or ids of catalog-native rows) of real tracks without a
/// recorded audio file, deduplicated.
pub(super) fn ids_missing_audio(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT COALESCE(catalog_id, id) FROM tracks          WHERE audio_path IS NULL AND is_custom = 0",
    )?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ----- Playlists / albums -----------------------------------------------------

pub(super) const PLAYLIST_COLUMNS: &str =
    "id, kind, name, artist, artwork_url, metadata, is_custom, description, genre, release_date";

pub(super) fn playlist_from_row(row: &Row<'_>) -> rusqlite::Result<Playlist> {
    let kind: String = row.get(1)?;
    Ok(Playlist {
        id: row.get(0)?,
        kind: CollectionKind::parse(&kind)
            .ok_or_else(|| conversion_error(1, format!("unknown collection kind {kind:?}")))?,
        name: row.get(2)?,
        artist: row.get(3)?,
        artwork_url: row.get(4)?,
        metadata: row.get(5)?,
        is_custom: row.get(6)?,
        description: row.get(7)?,
        genre: row.get(8)?,
        release_date: row.get(9)?,
    })
}

pub(super) fn upsert_playlist(conn: &Connection, playlist: &Playlist) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO playlists
           (id, kind, name, artist, artwork_url, metadata, is_custom, description, genre,
            release_date, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
           kind = excluded.kind, name = excluded.name, artist = excluded.artist,
           artwork_url = excluded.artwork_url, metadata = excluded.metadata,
           is_custom = excluded.is_custom, description = excluded.description,
           genre = excluded.genre, release_date = excluded.release_date,
           updated_at = excluded.updated_at",
        params![
            playlist.id,
            playlist.kind.as_str(),
            playlist.name,
            playlist.artist,
            playlist.artwork_url,
            playlist.metadata,
            playlist.is_custom,
            playlist.description,
            playlist.genre,
            playlist.release_date,
            now_unix()
        ],
    )?;
    Ok(())
}

pub(super) fn get_playlist(conn: &Connection, id: &str) -> Result<Option<Playlist>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {PLAYLIST_COLUMNS} FROM playlists WHERE id = ?1"),
            [id],
            playlist_from_row,
        )
        .optional()?)
}

pub(super) fn list_playlists(
    conn: &Connection,
    kind: Option<CollectionKind>,
) -> Result<Vec<Playlist>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PLAYLIST_COLUMNS} FROM playlists
         WHERE ?1 IS NULL OR kind = ?1 ORDER BY name"
    ))?;
    let rows = stmt.query_map([kind.map(CollectionKind::as_str)], playlist_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn delete_playlist(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM playlists WHERE id = ?1", [id])? > 0)
}

pub(super) fn set_playlist_tracks(
    conn: &mut Connection,
    playlist_id: &str,
    track_ids: &[String],
) -> Result<(), StorageError> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM playlist_tracks WHERE playlist_id = ?1",
        [playlist_id],
    )?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO playlist_tracks (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
        )?;
        for (position, track_id) in (0_i64..).zip(track_ids) {
            insert.execute(params![playlist_id, position, track_id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn playlist_track_ids(
    conn: &Connection,
    playlist_id: &str,
) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn
        .prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position")?;
    let rows = stmt.query_map([playlist_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Inserts or moves `track_id` within `collection_id` to `position` (clamped to
/// the end), renumbering the ordered membership. This is the reorder primitive a
/// UI drag-and-drop calls.
pub(super) fn place_track_in_collection(
    conn: &mut Connection,
    collection_id: &str,
    track_id: &str,
    position: usize,
) -> Result<(), StorageError> {
    let mut ids = playlist_track_ids(conn, collection_id)?;
    ids.retain(|existing| existing != track_id);
    let position = position.min(ids.len());
    ids.insert(position, track_id.to_owned());
    set_playlist_tracks(conn, collection_id, &ids)
}

/// Removes `track_id` from `collection_id`, renumbering the rest; returns
/// whether it was present.
pub(super) fn remove_track_from_collection(
    conn: &mut Connection,
    collection_id: &str,
    track_id: &str,
) -> Result<bool, StorageError> {
    let mut ids = playlist_track_ids(conn, collection_id)?;
    let before = ids.len();
    ids.retain(|existing| existing != track_id);
    if ids.len() == before {
        return Ok(false);
    }
    set_playlist_tracks(conn, collection_id, &ids)?;
    Ok(true)
}

// ----- Track mappings ---------------------------------------------------------

pub(super) fn set_track_mapping(
    conn: &Connection,
    codec: &PathCodec,
    mapping: &TrackMapping,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO track_mappings (track_id, file_path, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(track_id) DO UPDATE SET file_path = excluded.file_path",
        params![
            mapping.track_id,
            codec.encode(&mapping.file_path)?,
            now_unix()
        ],
    )?;
    Ok(())
}

fn mapping_from_row(codec: &PathCodec) -> impl Fn(&Row<'_>) -> rusqlite::Result<TrackMapping> + '_ {
    move |row| {
        Ok(TrackMapping {
            track_id: row.get(0)?,
            file_path: codec.decode(&row.get::<_, String>(1)?),
        })
    }
}

pub(super) fn get_track_mapping(
    conn: &Connection,
    codec: &PathCodec,
    track_id: &str,
) -> Result<Option<TrackMapping>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT track_id, file_path FROM track_mappings WHERE track_id = ?1",
            [track_id],
            mapping_from_row(codec),
        )
        .optional()?)
}

pub(super) fn list_track_mappings(
    conn: &Connection,
    codec: &PathCodec,
) -> Result<Vec<TrackMapping>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT track_id, file_path FROM track_mappings ORDER BY track_id")?;
    let rows = stmt.query_map([], mapping_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn delete_track_mapping(
    conn: &Connection,
    track_id: &str,
) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM track_mappings WHERE track_id = ?1", [track_id])? > 0)
}

// ----- Custom items (rich API over the unified `tracks` / `playlists` model) ---

type AudioColumns<'a> = (Option<&'static str>, Option<String>, Option<&'a str>);

fn audio_columns<'a>(
    codec: &PathCodec,
    audio: &'a CustomAudio,
) -> Result<AudioColumns<'a>, StorageError> {
    // (audio_mode, audio_path, reference_track_id)
    Ok(match audio {
        CustomAudio::None => (None, None, None),
        CustomAudio::LocalCopy(path) => (
            Some(CustomAudioMode::LocalCopy.as_str()),
            Some(codec.encode(path)?),
            None,
        ),
        CustomAudio::Reference(id) => (Some(CustomAudioMode::Reference.as_str()), None, Some(id)),
    })
}

fn custom_track_to_track(id: String, new: &NewCustomTrack) -> Result<Track, StorageError> {
    let audio_path = match &new.audio {
        CustomAudio::LocalCopy(path) => Some(PathBuf::from(path_str(path)?)),
        CustomAudio::None | CustomAudio::Reference(_) => None,
    };
    let (audio_mode, reference_track_id) = match &new.audio {
        CustomAudio::None => (None, None),
        CustomAudio::LocalCopy(_) => (Some(CustomAudioMode::LocalCopy), None),
        CustomAudio::Reference(catalog_id) => {
            (Some(CustomAudioMode::Reference), Some(catalog_id.clone()))
        }
    };
    Ok(Track {
        id,
        catalog_id: None,
        title: new.title.clone(),
        artist: new.artist.clone(),
        album: new.album.clone(),
        duration_ms: new.duration_ms,
        artwork_url: new.artwork_url.clone(),
        metadata: new.metadata.clone(),
        album_artist: new.album_artist.clone(),
        composer: new.composer.clone(),
        genre: new.genre.clone(),
        track_number: new.track_number,
        disc_number: new.disc_number,
        release_date: new.release_date.clone(),
        isrc: new.isrc.clone(),
        url: new.url.clone(),
        has_lyrics: new.has_lyrics,
        has_time_synced_lyrics: new.has_time_synced_lyrics,
        content_rating: new.content_rating.clone(),
        is_custom: true,
        real_album_id: new.real_album_id.clone(),
        audio_mode,
        reference_track_id,
        audio_path,
        lyrics_source: LyricsSource::None,
        lyrics_quality: LyricsQuality::None,
        in_library: false,
    })
}

/// Creates a custom song as a `tracks` row with `is_custom = 1` and id
/// `local-<n>`; returns the new id.
pub(super) fn create_custom_track(
    conn: &Connection,
    codec: &PathCodec,
    new: &NewCustomTrack,
) -> Result<String, StorageError> {
    let n = next_seq(conn, "song")?;
    let id = format!("{CUSTOM_SONG_ID_PREFIX}{n}");
    let track = custom_track_to_track(id.clone(), new)?;
    upsert_track(conn, codec, &track)?;
    Ok(id)
}

/// Rewrites a custom song's editable fields (audio included); returns whether a
/// custom row with that id existed.
pub(super) fn update_custom_track(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    new: &NewCustomTrack,
) -> Result<bool, StorageError> {
    let (audio_mode, audio_path, reference_track_id) = audio_columns(codec, &new.audio)?;
    Ok(conn.execute(
        "UPDATE tracks SET
           title = ?2, artist = ?3, album = ?4, duration_ms = ?5, artwork_url = ?6,
           metadata = COALESCE(?7, metadata), album_artist = ?8, composer = ?9, genre = ?10,
           track_number = ?11, disc_number = ?12, release_date = ?13, isrc = ?14, url = ?15,
           has_lyrics = ?16, has_time_synced_lyrics = ?17, content_rating = ?18,
           real_album_id = ?19, audio_mode = ?20, audio_path = ?21, reference_track_id = ?22,
           updated_at = ?23
         WHERE id = ?1 AND is_custom = 1",
        params![
            id,
            new.title,
            new.artist,
            new.album,
            new.duration_ms,
            new.artwork_url,
            new.metadata,
            new.album_artist,
            new.composer,
            new.genre,
            new.track_number,
            new.disc_number,
            new.release_date,
            new.isrc,
            new.url,
            new.has_lyrics,
            new.has_time_synced_lyrics,
            new.content_rating,
            new.real_album_id,
            audio_mode,
            audio_path,
            reference_track_id,
            now_unix()
        ],
    )? > 0)
}

/// Sets a custom song's audio provenance; returns whether a custom row existed.
pub(super) fn set_custom_track_audio(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    audio: &CustomAudio,
) -> Result<bool, StorageError> {
    let (audio_mode, audio_path, reference_track_id) = audio_columns(codec, audio)?;
    Ok(conn.execute(
        "UPDATE tracks SET audio_mode = ?2, audio_path = ?3, reference_track_id = ?4,
           updated_at = ?5
         WHERE id = ?1 AND is_custom = 1",
        params![id, audio_mode, audio_path, reference_track_id, now_unix()],
    )? > 0)
}

/// Lists every custom song (ordered by allocation).
pub(super) fn list_custom_tracks(
    conn: &Connection,
    codec: &PathCodec,
) -> Result<Vec<Track>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS} FROM tracks WHERE is_custom = 1
         ORDER BY CAST(substr(id, length('{CUSTOM_SONG_ID_PREFIX}') + 1) AS INTEGER)"
    ))?;
    let rows = stmt.query_map([], track_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Custom songs tagged into a given real album, ordered by track number.
pub(super) fn tracks_in_real_album(
    conn: &Connection,
    codec: &PathCodec,
    real_album_id: &str,
) -> Result<Vec<Track>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS} FROM tracks WHERE real_album_id = ?1
         ORDER BY disc_number, track_number, title"
    ))?;
    let rows = stmt.query_map([real_album_id], track_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Creates a custom album / playlist row (`is_custom = 1`), with an id
/// `local-al-<n>` or `local-pl-<n>`; returns the new id.
pub(super) fn create_custom_collection(
    conn: &Connection,
    new: &NewCustomCollection,
) -> Result<String, StorageError> {
    let n = next_seq(conn, new.kind.seq_name())?;
    let id = format!("{}{n}", new.kind.custom_id_prefix());
    let playlist = Playlist {
        id: id.clone(),
        kind: new.kind,
        name: new.name.clone(),
        artist: new.artist.clone(),
        artwork_url: new.artwork_url.clone(),
        metadata: new.metadata.clone(),
        is_custom: true,
        description: new.description.clone(),
        genre: new.genre.clone(),
        release_date: new.release_date.clone(),
    };
    upsert_playlist(conn, &playlist)?;
    Ok(id)
}

/// Rewrites a custom collection's editable fields (kind unchanged); returns
/// whether a custom row with that id existed.
pub(super) fn update_custom_collection(
    conn: &Connection,
    id: &str,
    new: &NewCustomCollection,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE playlists SET
           name = ?2, artist = ?3, artwork_url = ?4, metadata = COALESCE(?5, metadata),
           description = ?6, genre = ?7, release_date = ?8, updated_at = ?9
         WHERE id = ?1 AND is_custom = 1",
        params![
            id,
            new.name,
            new.artist,
            new.artwork_url,
            new.metadata,
            new.description,
            new.genre,
            new.release_date,
            now_unix()
        ],
    )? > 0)
}

/// Lists every custom album / playlist, ordered by name.
pub(super) fn list_custom_collections(conn: &Connection) -> Result<Vec<Playlist>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PLAYLIST_COLUMNS} FROM playlists WHERE is_custom = 1 ORDER BY name"
    ))?;
    let rows = stmt.query_map([], playlist_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ----- Auth tokens ------------------------------------------------------------

pub(super) fn set_auth_token(
    conn: &Connection,
    kind: TokenKind,
    value: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO auth_tokens (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![kind.key(), value, now_unix()],
    )?;
    Ok(())
}

pub(super) fn get_auth_token(
    conn: &Connection,
    kind: TokenKind,
) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT value FROM auth_tokens WHERE key = ?1",
            [kind.key()],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn delete_auth_token(conn: &Connection, kind: TokenKind) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM auth_tokens WHERE key = ?1", [kind.key()])? > 0)
}
