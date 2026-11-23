//! One row per song: song ids, aliases and how Apple ids are attached.
//!
//! Apple names one song by several ids: its catalog id (`123`, type `songs`)
//! and library ids (`i.…` for a saved song, `a.123` inside a library
//! playlist). `tracks` holds one row per song (see migration 0002). Its id is
//! the song id:
//!
//! - the catalog id when one is known: downloads, pins, media files and
//!   lyrics files are all keyed on it;
//! - else the library id the song was first seen under;
//! - `local-<n>` for a custom song.
//!
//! Every other id is a `track_aliases` row with the resource Apple returned
//! under it. Triggers keep song ids and aliases disjoint, so an id names at
//! most one song.
//!
//! [`claim`] is the one write path that attaches ids. When a library id learns
//! its catalog id, its song is re-keyed to the catalog id ([`rekey`]), or
//! merged into the catalog song when that exists already ([`merge`]). Keyed
//! rows outside `tracks` (pins, spatial facts, local-file links, custom
//! references) follow in both cases. Each re-key or merge is reported as a
//! [`SongRekey`]: files named after the old id (lyrics) and audio files a
//! merge dropped are moved or settled after the change is committed, outside
//! the database (see [`crate::Library`]).

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::StorageError;

/// A song that took another id: [`rekey`] or [`merge`] moved song `from`
/// to `into` (`from` is now an alias of `into`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SongRekey {
    /// The song's previous id.
    pub from: String,
    /// The song id it goes by now.
    pub into: String,
    /// Audio files `from` recorded for a mix `into` had a different file
    /// for: the merge kept `into`'s row and dropped `from`'s.
    pub dropped_audio: Vec<DroppedAudio>,
}

/// An audio row a merge dropped (see [`SongRekey::dropped_audio`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DroppedAudio {
    /// The mix (`track_audio.variant`).
    pub variant: String,
    /// Stored path of the file the song kept for that mix.
    pub kept: String,
    /// Stored path of the file the dropped row recorded.
    pub dropped: String,
    /// The dropped row's size in bytes.
    pub size: Option<i64>,
    /// The dropped row's duration in milliseconds.
    pub duration_ms: Option<i64>,
}

/// SQL expression: the song id the Apple id in parameter `?1` names (the
/// id itself when it is not an alias).
pub(super) const SONG_OF_P1: &str =
    "COALESCE((SELECT sa.track_id FROM track_aliases sa WHERE sa.alias = ?1), ?1)";

/// Whether `id` is a catalog (adam) id: all ASCII digits.
#[must_use]
pub(crate) fn is_catalog_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
}

/// The song the Apple id `id` names, if stored: `id` itself when it is a song
/// id, the song of the alias otherwise.
pub(super) fn song_of(conn: &Connection, id: &str) -> Result<Option<String>, StorageError> {
    Ok(conn
        .prepare_cached(
            "SELECT id FROM tracks WHERE id = ?1
             UNION ALL SELECT track_id FROM track_aliases WHERE alias = ?1
             LIMIT 1",
        )?
        .query_row([id], |row| row.get(0))
        .optional()?)
}

/// The song id for a resource observed under `id`, which names catalog id
/// `catalog` (`playParams.catalogId`) when given. Existing songs are joined
/// up first, so the returned id names the one song all of them belong to:
///
/// - a song keyed on a library id that now has a catalog id is re-keyed to
///   it, or merged into the song of that catalog id when that exists;
/// - a library id that pointed at another catalog song is moved over (its
///   alias row is rewritten by the caller's [`put_alias`]);
/// - two catalog songs are never merged.
///
/// The caller then upserts the song row and, when `id` differs from the
/// returned song id, records `id` as its alias. A re-key or merge is added
/// to `rekeys`; the caller reports them once its transaction is committed.
pub(super) fn claim(
    conn: &Connection,
    id: &str,
    catalog: Option<&str>,
    rekeys: &mut Vec<SongRekey>,
) -> Result<String, StorageError> {
    let own = song_of(conn, id)?;
    let Some(catalog) = catalog.filter(|c| !c.is_empty() && *c != id) else {
        return Ok(own.unwrap_or_else(|| id.to_owned()));
    };
    let twin = song_of(conn, catalog)?;
    let stored = twin.is_some();
    let target = twin.unwrap_or_else(|| catalog.to_owned());
    Ok(match own {
        Some(a) if a == target => a,
        // A catalog song keeps its identity: a resource that is that song
        // stays, and only another id of it (the alias `id`) moves.
        Some(a) if is_catalog_id(&a) => {
            if a == id {
                a
            } else {
                target
            }
        }
        Some(a) => {
            let dropped_audio = if stored {
                merge(conn, &a, &target)?
            } else {
                rekey(conn, &a, &target)?;
                Vec::new()
            };
            rekeys.push(SongRekey {
                from: a,
                into: target.clone(),
                dropped_audio,
            });
            target
        }
        None => target,
    })
}

/// Records `alias` as another id of song `song` with the resource Apple
/// returned under it. A known alias moves to `song`; its stored resource is
/// kept when `metadata` is `None`, and its library flag is sticky.
pub(super) fn put_alias(
    conn: &Connection,
    alias: &str,
    song: &str,
    metadata: Option<&str>,
    in_library: bool,
) -> Result<(), StorageError> {
    conn.prepare_cached(
        "INSERT INTO track_aliases (alias, track_id, metadata, in_library)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (alias) DO UPDATE SET
           track_id = excluded.track_id,
           metadata = COALESCE(excluded.metadata, track_aliases.metadata),
           in_library = MAX(track_aliases.in_library, excluded.in_library)",
    )?
    .execute(params![alias, song, metadata, in_library])?;
    // Rows written under the id before it was known to be this song's.
    move_rows(conn, alias, song)?;
    Ok(())
}

/// The `id` of a JSON resource, if it has one.
pub(super) fn resource_id(metadata: &str) -> Option<String> {
    serde_json::from_str::<Value>(metadata)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

/// The stored resource and library flag of song `song`.
fn resource_and_flag(
    conn: &Connection,
    song: &str,
) -> Result<(Option<String>, bool), StorageError> {
    Ok(conn.query_row(
        "SELECT metadata, in_library FROM tracks WHERE id = ?1",
        [song],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// Changes song `old`'s id to `new` (no song or alias has it yet): its audio
/// and aliases follow through `ON UPDATE CASCADE`, `old` becomes an alias
/// with the resource stored under it, and keyed rows move along.
fn rekey(conn: &Connection, old: &str, new: &str) -> Result<(), StorageError> {
    let (metadata, in_library) = resource_and_flag(conn, old)?;
    conn.execute(
        "UPDATE tracks SET id = ?2, metadata = NULL, updated_at = ?3 WHERE id = ?1",
        params![old, new, super::repo::now_unix()],
    )?;
    put_alias(conn, old, new, metadata.as_deref(), in_library)?;
    move_keyed(conn, old, new)?;
    tracing::debug!(%old, %new, "song re-keyed to its catalog id");
    Ok(())
}

/// Merges song `from` (keyed on a library id) into song `into`: columns
/// `into` lacks are filled from `from`, audio `into` lacks moves over, the
/// better lyrics state wins (custom first), aliases and keyed rows move, and
/// `from` becomes an alias of `into`. Returns the audio rows of `from` that
/// were dropped because `into` has a different file for the same mix.
fn merge(conn: &Connection, from: &str, into: &str) -> Result<Vec<DroppedAudio>, StorageError> {
    conn.execute(
        "UPDATE tracks SET
           album = COALESCE(tracks.album, f.album),
           duration_ms = COALESCE(tracks.duration_ms, f.duration_ms),
           artwork_url = COALESCE(tracks.artwork_url, f.artwork_url),
           album_artist = COALESCE(tracks.album_artist, f.album_artist),
           composer = COALESCE(tracks.composer, f.composer),
           genre = COALESCE(tracks.genre, f.genre),
           track_number = COALESCE(tracks.track_number, f.track_number),
           disc_number = COALESCE(tracks.disc_number, f.disc_number),
           release_date = COALESCE(tracks.release_date, f.release_date),
           isrc = COALESCE(tracks.isrc, f.isrc),
           url = COALESCE(tracks.url, f.url),
           content_rating = COALESCE(tracks.content_rating, f.content_rating),
           has_lyrics = MAX(tracks.has_lyrics, f.has_lyrics),
           has_time_synced_lyrics = MAX(tracks.has_time_synced_lyrics, f.has_time_synced_lyrics),
           in_library = MAX(tracks.in_library, f.in_library),
           audio_mode = COALESCE(tracks.audio_mode, f.audio_mode),
           lyrics_quality = CASE
             WHEN (tracks.lyrics_source = 'none' AND f.lyrics_source <> 'none')
               OR (f.lyrics_source = 'custom' AND tracks.lyrics_source <> 'custom')
             THEN f.lyrics_quality ELSE tracks.lyrics_quality END,
           lyrics_source = CASE
             WHEN (tracks.lyrics_source = 'none' AND f.lyrics_source <> 'none')
               OR (f.lyrics_source = 'custom' AND tracks.lyrics_source <> 'custom')
             THEN f.lyrics_source ELSE tracks.lyrics_source END,
           updated_at = MAX(tracks.updated_at, f.updated_at)
         FROM (SELECT * FROM tracks WHERE id = ?1) AS f
         WHERE tracks.id = ?2",
        params![from, into],
    )?;
    let dropped = conn
        .prepare(
            "SELECT f.variant, i.path, f.path, f.size, f.duration_ms
             FROM track_audio f
             JOIN track_audio i ON i.track_id = ?2 AND i.variant = f.variant
             WHERE f.track_id = ?1 AND f.path <> i.path",
        )?
        .query_map(params![from, into], |row| {
            Ok(DroppedAudio {
                variant: row.get(0)?,
                kept: row.get(1)?,
                dropped: row.get(2)?,
                size: row.get(3)?,
                duration_ms: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    conn.execute(
        "INSERT OR IGNORE INTO track_audio (track_id, variant, path, size, duration_ms, created_at)
         SELECT ?2, variant, path, size, duration_ms, created_at
         FROM track_audio WHERE track_id = ?1",
        params![from, into],
    )?;
    conn.execute(
        "UPDATE track_aliases SET track_id = ?2 WHERE track_id = ?1",
        params![from, into],
    )?;
    let (metadata, in_library) = resource_and_flag(conn, from)?;
    conn.execute("DELETE FROM tracks WHERE id = ?1", [from])?;
    put_alias(conn, from, into, metadata.as_deref(), in_library)?;
    move_keyed(conn, from, into)?;
    tracing::debug!(%from, %into, dropped = dropped.len(), "song merged into its catalog twin");
    Ok(dropped)
}

/// Moves every row keyed on song id `from` to `into`: pins, spatial facts,
/// links to local files and custom songs' references. Where `into` has a row
/// of its own, that one is kept. Queue rows stay: a worker may be running
/// under the row id, and the queue resolves whatever id a row names.
fn move_keyed(conn: &Connection, from: &str, into: &str) -> Result<(), StorageError> {
    move_rows(conn, from, into)?;
    conn.prepare_cached("UPDATE tracks SET reference_track_id = ?2 WHERE reference_track_id = ?1")?
        .execute(params![from, into])?;
    Ok(())
}

/// [`move_keyed`] without the custom references (those are resolved through
/// the aliases when read, and finding them takes a scan).
fn move_rows(conn: &Connection, from: &str, into: &str) -> Result<(), StorageError> {
    for table in ["download_pins", "song_spatial", "track_mappings"] {
        conn.prepare_cached(&format!(
            "UPDATE OR IGNORE {table} SET track_id = ?2 WHERE track_id = ?1"
        ))?
        .execute(params![from, into])?;
        conn.prepare_cached(&format!("DELETE FROM {table} WHERE track_id = ?1"))?
            .execute([from])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        super::super::migrations::migrate(&mut conn).unwrap();
        conn
    }

    fn insert(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO tracks (id, title, artist, updated_at) VALUES (?1, 'T', 'A', 1)",
            [id],
        )
        .unwrap();
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn claim_should_use_the_catalog_id_for_a_new_library_song() {
        let conn = db();
        assert_eq!(
            claim(&conn, "i.a", Some("123"), &mut Vec::new()).unwrap(),
            "123"
        );
    }

    #[test]
    fn claim_should_join_a_library_id_to_its_stored_catalog_song() {
        let conn = db();
        insert(&conn, "123");
        assert_eq!(
            claim(&conn, "a.123", Some("123"), &mut Vec::new()).unwrap(),
            "123"
        );
    }

    #[test]
    fn claim_should_rekey_a_library_song_once_its_catalog_id_is_known() {
        let conn = db();
        insert(&conn, "i.a");
        conn.execute(
            "INSERT INTO track_audio (track_id, variant, path, created_at)
             VALUES ('i.a', 'stereo', 'media/x.m4a', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO download_pins (track_id, owner, created_at) VALUES ('i.a', 'p.1', 1)",
            [],
        )
        .unwrap();
        assert_eq!(
            claim(&conn, "i.a", Some("123"), &mut Vec::new()).unwrap(),
            "123"
        );
        assert_eq!(
            (
                count(&conn, "SELECT COUNT(*) FROM tracks WHERE id = '123'"),
                count(
                    &conn,
                    "SELECT COUNT(*) FROM track_audio WHERE track_id = '123'"
                ),
                count(
                    &conn,
                    "SELECT COUNT(*) FROM download_pins WHERE track_id = '123'"
                ),
                count(
                    &conn,
                    "SELECT COUNT(*) FROM track_aliases WHERE alias = 'i.a'"
                ),
            ),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn claim_should_merge_a_library_song_into_its_catalog_twin() {
        let conn = db();
        insert(&conn, "i.a");
        insert(&conn, "123");
        conn.execute(
            "UPDATE tracks SET lyrics_source = 'custom', lyrics_quality = 'line' WHERE id = 'i.a'",
            [],
        )
        .unwrap();
        assert_eq!(
            claim(&conn, "i.a", Some("123"), &mut Vec::new()).unwrap(),
            "123"
        );
        assert_eq!(
            (
                count(&conn, "SELECT COUNT(*) FROM tracks"),
                count(
                    &conn,
                    "SELECT COUNT(*) FROM tracks WHERE lyrics_source = 'custom'"
                ),
            ),
            (1, 1)
        );
    }

    #[test]
    fn claim_should_never_merge_two_catalog_songs() {
        let conn = db();
        insert(&conn, "111");
        insert(&conn, "222");
        put_alias(&conn, "i.a", "111", None, false).unwrap();
        assert_eq!(
            claim(&conn, "i.a", Some("222"), &mut Vec::new()).unwrap(),
            "222"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM tracks"), 2);
    }

    #[test]
    fn an_alias_should_never_be_a_song_id() {
        let conn = db();
        insert(&conn, "123");
        assert!(put_alias(&conn, "123", "123", None, false).is_err());
    }

    #[test]
    fn a_song_id_should_never_be_an_alias() {
        let conn = db();
        insert(&conn, "123");
        put_alias(&conn, "i.a", "123", None, false).unwrap();
        assert!(
            conn.execute(
                "INSERT INTO tracks (id, title, artist, updated_at) VALUES ('i.a', 'T', 'A', 1)",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn deleting_a_song_should_delete_its_aliases() {
        let conn = db();
        insert(&conn, "123");
        put_alias(&conn, "i.a", "123", None, false).unwrap();
        conn.execute("DELETE FROM tracks WHERE id = '123'", [])
            .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM track_aliases"), 0);
    }
}
