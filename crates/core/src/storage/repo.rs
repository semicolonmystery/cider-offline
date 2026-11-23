//! Synchronous repository functions. Called on a blocking thread by
//! [`super::Storage`]; never call these from async code directly.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::StorageError;
use super::models::{
    AudioFile, AudioVariant, CUSTOM_SONG_ID_PREFIX, CacheEntry, CollectionKind, CustomAudio,
    CustomAudioMode, LyricsQuality, LyricsSource, NewCustomCollection, NewCustomTrack, Playlist,
    TokenKind, Track, TrackAudio, TrackMapping,
};
use super::paths::PathCodec;
use super::songs::{self, SONG_OF_P1};

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

/// Columns of [`Track`], in `track_from_row` order, over a `tracks` row
/// aliased `t` (every query selecting them must say `FROM tracks t`, or name
/// an equivalent subquery `t`). The audio variants come from `track_audio`.
///
/// `metadata` is the song's own resource (the catalog one), else the resource
/// of one of its aliases (a saved library copy first), so callers always see
/// what Apple said about the song. `catalog_id` is the song id when that is a
/// catalog id: all digits, like [`super::songs::is_catalog_id`].
pub(super) const TRACK_COLUMNS: &str = "t.id, t.title, t.artist, t.album, t.duration_ms, \
     t.artwork_url, COALESCE(t.metadata, (SELECT ta.metadata FROM track_aliases ta \
       WHERE ta.track_id = t.id AND ta.metadata IS NOT NULL \
       ORDER BY ta.in_library DESC, ta.alias LIMIT 1)), \
     t.album_artist, t.composer, t.genre, t.track_number, \
     t.disc_number, t.release_date, t.isrc, t.url, t.has_lyrics, t.has_time_synced_lyrics, \
     t.content_rating, t.is_custom, t.real_album_id, t.audio_mode, t.reference_track_id, \
     (SELECT a.path FROM track_audio a WHERE a.track_id = t.id AND a.variant = 'stereo'), \
     t.lyrics_source, CASE WHEN t.id <> '' AND t.id NOT GLOB '*[^0-9]*' THEN t.id END, \
     t.in_library, t.lyrics_quality, \
     (SELECT a.path FROM track_audio a WHERE a.track_id = t.id AND a.variant = 'spatial')";

/// Number of [`TRACK_COLUMNS`]; a query may append its own columns after them.
pub(super) const TRACK_COLUMN_COUNT: usize = 28;

/// SQL predicate: the track row `t` has at least one audio variant.
pub(super) const HAS_AUDIO: &str = "EXISTS (SELECT 1 FROM track_audio a WHERE a.track_id = t.id)";

/// Row mapper for [`TRACK_COLUMNS`], resolving audio paths through `codec`.
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
        audio: TrackAudio {
            normal: row
                .get::<_, Option<String>>(22)?
                .map(|stored| codec.decode(&stored)),
            spatial: row
                .get::<_, Option<String>>(27)?
                .map(|stored| codec.decode(&stored)),
        },
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
    rekeys: &mut Vec<songs::SongRekey>,
) -> Result<(), StorageError> {
    // No lyrics means no quality (a CHECK on `tracks`).
    let lyrics_quality = if track.lyrics_source == LyricsSource::None {
        LyricsQuality::None
    } else {
        track.lyrics_quality
    };
    // `track.id` may be any id of the song; its columns land on the song row.
    let song = songs::claim(conn, &track.id, track.catalog_id.as_deref(), rekeys)?;
    let own_resource = song == track.id;
    // `metadata` is the resource under `track.id`. On the song row it is
    // stored only when it describes the song itself: a `Track` read back
    // carries an alias's resource when the song has none of its own.
    let song_metadata = track.metadata.as_deref().filter(|metadata| {
        own_resource && songs::resource_id(metadata).is_none_or(|id| id == song)
    });
    conn.execute(
        "INSERT INTO tracks
           (id, title, artist, album, duration_ms, artwork_url, metadata, album_artist, composer,
            genre, track_number, disc_number, release_date, isrc, url, has_lyrics,
            has_time_synced_lyrics, content_rating, is_custom, real_album_id, audio_mode,
            reference_track_id, lyrics_source, in_library, updated_at, lyrics_quality)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)
         ON CONFLICT(id) DO UPDATE SET
           title = excluded.title, artist = excluded.artist, album = excluded.album,
           duration_ms = excluded.duration_ms, artwork_url = excluded.artwork_url,
           metadata = CASE WHEN ?27 THEN excluded.metadata ELSE tracks.metadata END,
           album_artist = excluded.album_artist,
           composer = excluded.composer, genre = excluded.genre,
           track_number = excluded.track_number, disc_number = excluded.disc_number,
           release_date = excluded.release_date, isrc = excluded.isrc, url = excluded.url,
           has_lyrics = excluded.has_lyrics,
           has_time_synced_lyrics = excluded.has_time_synced_lyrics,
           content_rating = excluded.content_rating, is_custom = excluded.is_custom,
           real_album_id = excluded.real_album_id, audio_mode = excluded.audio_mode,
           reference_track_id = excluded.reference_track_id,
           lyrics_source = excluded.lyrics_source,
           lyrics_quality = excluded.lyrics_quality,
           -- Sticky: a later leaner capture never clears a proven membership.
           in_library = MAX(tracks.in_library, excluded.in_library),
           updated_at = excluded.updated_at",
        params![
            song,
            track.title,
            track.artist,
            track.album,
            track.duration_ms,
            track.artwork_url,
            song_metadata,
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
            track.lyrics_source.as_str(),
            track.in_library,
            now_unix(),
            lyrics_quality.as_str(),
            song_metadata.is_some() || (own_resource && track.metadata.is_none()),
        ],
    )?;
    if !own_resource {
        songs::put_alias(
            conn,
            &track.id,
            &song,
            track.metadata.as_deref(),
            track.in_library,
        )?;
    }
    // The song's audio is exactly `track.audio`, as when it was a column.
    for variant in AudioVariant::ALL {
        let file = track
            .audio
            .get(variant)
            .map(|path| AudioFile::new(variant, path));
        put_audio(conn, codec, &song, variant, file.as_ref())?;
    }
    Ok(())
}

pub(super) fn get_track(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
) -> Result<Option<Track>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {TRACK_COLUMNS} FROM tracks t WHERE t.id = {SONG_OF_P1}"),
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
        "SELECT {TRACK_COLUMNS} FROM tracks t ORDER BY t.artist, t.album, t.title"
    ))?;
    let rows = stmt.query_map([], track_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn delete_track(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute(&format!("DELETE FROM tracks WHERE id = {SONG_OF_P1}"), [id])? > 0)
}

pub(super) fn set_track_audio(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    variant: AudioVariant,
    audio_path: Option<&Path>,
) -> Result<bool, StorageError> {
    let Some(song) = songs::song_of(conn, id)? else {
        return Ok(false);
    };
    conn.execute(
        "UPDATE tracks SET updated_at = ?2 WHERE id = ?1",
        params![song, now_unix()],
    )?;
    let file = audio_path.map(|path| AudioFile::new(variant, path));
    put_audio(conn, codec, &song, variant, file.as_ref())?;
    Ok(true)
}

/// Records `file` as the `variant` audio of track row `track_id`, or deletes
/// that variant with `None`. Re-recording the same path keeps its size,
/// duration and timestamp unless new ones are given.
fn put_audio(
    conn: &Connection,
    codec: &PathCodec,
    track_id: &str,
    variant: AudioVariant,
    file: Option<&AudioFile>,
) -> Result<(), StorageError> {
    let Some(file) = file else {
        conn.prepare_cached("DELETE FROM track_audio WHERE track_id = ?1 AND variant = ?2")?
            .execute(params![track_id, variant.as_str()])?;
        return Ok(());
    };
    // A size past i64::MAX cannot exist; record it as unknown.
    let size = file.size.and_then(|size| i64::try_from(size).ok());
    conn.prepare_cached(
        "INSERT INTO track_audio (track_id, variant, path, size, duration_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (track_id, variant) DO UPDATE SET
           path = excluded.path,
           size = CASE WHEN excluded.path = track_audio.path
                       THEN COALESCE(excluded.size, track_audio.size) ELSE excluded.size END,
           duration_ms = CASE WHEN excluded.path = track_audio.path
                              THEN COALESCE(excluded.duration_ms, track_audio.duration_ms)
                              ELSE excluded.duration_ms END,
           created_at = CASE WHEN excluded.path = track_audio.path
                             THEN track_audio.created_at ELSE excluded.created_at END",
    )?
    .execute(params![
        track_id,
        variant.as_str(),
        codec.encode(&file.path)?,
        size,
        file.duration_ms,
        now_unix()
    ])?;
    Ok(())
}

/// The song `id` names (as a one-element list, empty when unknown), after
/// bumping its `updated_at` (an audio change is a change of the track).
fn touch_any(conn: &Connection, id: &str) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare_cached(&format!(
        "UPDATE tracks SET updated_at = ?2 WHERE id = {SONG_OF_P1} RETURNING id"
    ))?;
    let rows = stmt.query_map(params![id, now_unix()], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records `file` (or deletes `variant` with `None`) on the song any of whose
/// ids is `id`; returns how many songs matched (0 or 1).
pub(super) fn put_audio_any(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    variant: AudioVariant,
    file: Option<&AudioFile>,
) -> Result<usize, StorageError> {
    let rows = touch_any(conn, id)?;
    for track_id in &rows {
        put_audio(conn, codec, track_id, variant, file)?;
    }
    Ok(rows.len())
}

/// Deletes every audio variant of the non-custom song any of whose ids is
/// `id`; returns how many audio rows were deleted. A custom song's audio is
/// the user's own and is never cleared here.
pub(super) fn clear_audio_any(conn: &Connection, id: &str) -> Result<usize, StorageError> {
    let deleted = conn
        .prepare_cached(&format!(
            "DELETE FROM track_audio WHERE track_id IN
           (SELECT t.id FROM tracks t WHERE t.id = {SONG_OF_P1} AND t.is_custom = 0)"
        ))?
        .execute([id])?;
    if deleted > 0 {
        conn.prepare_cached(&format!(
            "UPDATE tracks SET updated_at = ?2 WHERE id = {SONG_OF_P1}"
        ))?
        .execute(params![id, now_unix()])?;
    }
    Ok(deleted)
}

/// The audio files recorded for track row `track_id`, normal first.
pub(super) fn track_audio_files(
    conn: &Connection,
    codec: &PathCodec,
    track_id: &str,
) -> Result<Vec<AudioFile>, StorageError> {
    let mut stmt = conn.prepare_cached(
        "SELECT variant, path, size, duration_ms FROM track_audio
         WHERE track_id = (SELECT COALESCE((SELECT sa.track_id FROM track_aliases sa
                                           WHERE sa.alias = ?1), ?1))
         ORDER BY variant = 'spatial', variant",
    )?;
    let rows = stmt.query_map([track_id], |row| {
        let variant: String = row.get(0)?;
        Ok(AudioFile {
            variant: AudioVariant::parse(&variant)
                .ok_or_else(|| conversion_error(0, format!("unknown audio variant {variant:?}")))?,
            path: codec.decode(&row.get::<_, String>(1)?),
            size: row
                .get::<_, Option<i64>>(2)?
                .and_then(|size| u64::try_from(size).ok()),
            duration_ms: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn set_track_lyrics_source(
    conn: &Connection,
    id: &str,
    source: LyricsSource,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        // No lyrics means no quality (a CHECK on `tracks`).
        "UPDATE tracks SET lyrics_source = ?2,
           lyrics_quality = CASE WHEN ?2 = 'none' THEN 'none' ELSE lyrics_quality END,
           updated_at = ?3
         WHERE id = (SELECT COALESCE((SELECT sa.track_id FROM track_aliases sa
                                      WHERE sa.alias = ?1), ?1))",
        params![id, source.as_str(), now_unix()],
    )? > 0)
}

/// Sets the lyrics source and timing of the song any of whose ids is `id`;
/// returns how many rows changed (0 or 1).
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
        "UPDATE tracks SET lyrics_source = ?2, lyrics_quality = ?3, updated_at = ?4
         WHERE id = COALESCE((SELECT sa.track_id FROM track_aliases sa WHERE sa.alias = ?1), ?1)",
        params![id, source.as_str(), quality.as_str(), now_unix()],
    )?)
}

/// Song ids (download keys) of real songs without any recorded audio variant.
pub(super) fn ids_missing_audio(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT t.id FROM tracks t
         WHERE t.is_custom = 0 AND NOT {HAS_AUDIO}"
    ))?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The subset of `ids` that name a song with local audio: a recorded audio
/// file (any variant) or a track mapping. Any id of the song counts (its
/// catalog id, a library id, an `a.…` id). File existence is not checked.
pub(super) fn ids_with_local_audio(
    conn: &Connection,
    ids: &[String],
) -> Result<std::collections::HashSet<String>, StorageError> {
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS (SELECT 1 FROM track_audio a
                        WHERE a.track_id = COALESCE((SELECT sa.track_id FROM track_aliases sa
                                                     WHERE sa.alias = ?1), ?1))
             OR EXISTS (SELECT 1 FROM track_mappings m
                        WHERE m.track_id IN (?1, COALESCE((SELECT sa.track_id
                                                           FROM track_aliases sa
                                                           WHERE sa.alias = ?1), ?1)))",
    )?;
    let mut found = std::collections::HashSet::new();
    for id in ids {
        if stmt.query_row([id], |row| row.get::<_, bool>(0))? {
            found.insert(id.clone());
        }
    }
    Ok(found)
}

/// Which variants the songs named by `ids` have locally, for the ids that
/// have any: a track mapping counts as the normal variant. Any id of a song
/// counts; file existence is not checked.
pub(super) fn local_variants(
    conn: &Connection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<AudioVariant>>, StorageError> {
    let mut stmt = conn.prepare_cached(
        "SELECT
           EXISTS (SELECT 1 FROM track_audio a
                   WHERE a.track_id = COALESCE((SELECT sa.track_id FROM track_aliases sa
                                                WHERE sa.alias = ?1), ?1)
                     AND a.variant = 'stereo')
             OR EXISTS (SELECT 1 FROM track_mappings m
                        WHERE m.track_id IN (?1, COALESCE((SELECT sa.track_id
                                                           FROM track_aliases sa
                                                           WHERE sa.alias = ?1), ?1))),
           EXISTS (SELECT 1 FROM track_audio a
                   WHERE a.track_id = COALESCE((SELECT sa.track_id FROM track_aliases sa
                                                WHERE sa.alias = ?1), ?1)
                     AND a.variant = 'spatial')",
    )?;
    let mut found = std::collections::HashMap::new();
    for id in ids {
        let (normal, spatial) = stmt.query_row([id], |row| {
            Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?))
        })?;
        let variants: Vec<AudioVariant> = [
            (normal, AudioVariant::Stereo),
            (spatial, AudioVariant::Spatial),
        ]
        .into_iter()
        .filter_map(|(present, variant)| present.then_some(variant))
        .collect();
        if !variants.is_empty() {
            found.insert(id.clone(), variants);
        }
    }
    Ok(found)
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
        // Keyed on the song id, whichever id of the song is given.
        &format!(
            "INSERT INTO track_mappings (track_id, file_path, created_at)
             VALUES ({SONG_OF_P1}, ?2, ?3)
             ON CONFLICT(track_id) DO UPDATE SET file_path = excluded.file_path"
        ),
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
            &format!(
                "SELECT track_id, file_path FROM track_mappings
                 WHERE track_id IN (?1, {SONG_OF_P1}) ORDER BY track_id = ?1 DESC LIMIT 1"
            ),
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
    Ok(conn.execute(
        &format!("DELETE FROM track_mappings WHERE track_id IN (?1, {SONG_OF_P1})"),
        [track_id],
    )? > 0)
}

// ----- Custom items (rich API over the unified `tracks` / `playlists` model) ---

/// `(audio_mode, reference_track_id)` of a custom song's audio provenance.
fn audio_columns(audio: &CustomAudio) -> (Option<&'static str>, Option<&str>) {
    match audio {
        CustomAudio::None => (None, None),
        CustomAudio::LocalCopy(_) => (Some(CustomAudioMode::LocalCopy.as_str()), None),
        CustomAudio::Reference(id) => (Some(CustomAudioMode::Reference.as_str()), Some(id)),
    }
}

/// Makes a custom song's audio rows match `audio`: a local copy is its only
/// (normal) variant; reference mode and no audio record no file.
fn put_custom_audio(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    audio: &CustomAudio,
) -> Result<(), StorageError> {
    let normal = match audio {
        CustomAudio::LocalCopy(path) => Some(AudioFile::new(AudioVariant::Stereo, path)),
        CustomAudio::None | CustomAudio::Reference(_) => None,
    };
    put_audio(conn, codec, id, AudioVariant::Stereo, normal.as_ref())?;
    put_audio(conn, codec, id, AudioVariant::Spatial, None)
}

fn custom_track_to_track(id: String, new: &NewCustomTrack) -> Result<Track, StorageError> {
    let audio = match &new.audio {
        CustomAudio::LocalCopy(path) => TrackAudio::normal(PathBuf::from(path_str(path)?)),
        CustomAudio::None | CustomAudio::Reference(_) => TrackAudio::default(),
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
        audio,
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
    // A custom song has no catalog id, so nothing is re-keyed.
    upsert_track(conn, codec, &track, &mut Vec::new())?;
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
    let (audio_mode, reference_track_id) = audio_columns(&new.audio);
    let updated = conn.execute(
        "UPDATE tracks SET
           title = ?2, artist = ?3, album = ?4, duration_ms = ?5, artwork_url = ?6,
           metadata = COALESCE(?7, metadata), album_artist = ?8, composer = ?9, genre = ?10,
           track_number = ?11, disc_number = ?12, release_date = ?13, isrc = ?14, url = ?15,
           has_lyrics = ?16, has_time_synced_lyrics = ?17, content_rating = ?18,
           real_album_id = ?19, audio_mode = ?20, reference_track_id = ?21, updated_at = ?22
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
            reference_track_id,
            now_unix()
        ],
    )? > 0;
    if updated {
        put_custom_audio(conn, codec, id, &new.audio)?;
    }
    Ok(updated)
}

/// Sets a custom song's audio provenance; returns whether a custom row existed.
pub(super) fn set_custom_track_audio(
    conn: &Connection,
    codec: &PathCodec,
    id: &str,
    audio: &CustomAudio,
) -> Result<bool, StorageError> {
    let (audio_mode, reference_track_id) = audio_columns(audio);
    let updated = conn.execute(
        "UPDATE tracks SET audio_mode = ?2, reference_track_id = ?3, updated_at = ?4
         WHERE id = ?1 AND is_custom = 1",
        params![id, audio_mode, reference_track_id, now_unix()],
    )? > 0;
    if updated {
        put_custom_audio(conn, codec, id, audio)?;
    }
    Ok(updated)
}

/// Lists every custom song (ordered by allocation).
pub(super) fn list_custom_tracks(
    conn: &Connection,
    codec: &PathCodec,
) -> Result<Vec<Track>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS} FROM tracks t WHERE t.is_custom = 1
         ORDER BY CAST(substr(t.id, length('{CUSTOM_SONG_ID_PREFIX}') + 1) AS INTEGER)"
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
        "SELECT {TRACK_COLUMNS} FROM tracks t WHERE t.real_album_id = ?1
         ORDER BY t.disc_number, t.track_number, t.title"
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
