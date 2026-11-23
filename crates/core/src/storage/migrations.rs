//! Embedded, ordered schema migrations tracked with `PRAGMA user_version`.
//!
//! Migrations are append-only: a released one is applied on users' machines
//! and must never be edited. A schema change is a new numbered file.

use rusqlite::Connection;

use super::StorageError;

/// Migration scripts; index `i` upgrades the schema from version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_audio_mixes_and_downloads.sql"),
];

/// Schema version after all migrations ran.
pub const SCHEMA_VERSION: u32 = {
    assert!(MIGRATIONS.len() < u32::MAX as usize);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded by the assertion above"
    )]
    let version = MIGRATIONS.len() as u32;
    version
};

/// Reads the current schema version.
pub fn current_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Applies all pending migrations, each in its own transaction together with
/// its `user_version` bump, so a failed migration leaves the previous version
/// intact and a rerun resumes where it stopped.
///
/// Foreign keys are switched off while migrating (SQLite ignores the pragma
/// inside a transaction), as SQLite's table-rebuild procedure requires, and
/// every migration must leave `PRAGMA foreign_key_check` empty before it
/// commits. The connection's previous `foreign_keys` setting is restored.
///
/// # Errors
///
/// [`StorageError::SchemaTooNew`] if the database was written by a newer app;
/// [`StorageError::ForeignKeyViolation`] if a migration broke a reference;
/// otherwise database errors.
pub fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let current = current_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }
    let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = apply_pending(conn, current);
    // Restore even when a migration failed, so the connection stays usable.
    let restored = conn.pragma_update(None, "foreign_keys", foreign_keys);
    result?;
    restored?;
    Ok(())
}

fn apply_pending(conn: &mut Connection, current: u32) -> Result<(), StorageError> {
    for (version, sql) in (1..=SCHEMA_VERSION).zip(MIGRATIONS).skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        let violations = usize::try_from(violations).unwrap_or(usize::MAX);
        if violations > 0 {
            // Dropping `tx` rolls the migration back.
            return Err(StorageError::ForeignKeyViolation {
                version,
                violations,
            });
        }
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies migrations `1..=version` to a fresh in-memory database.
    fn db_at(version: u32) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for (v, sql) in (1..=version).zip(MIGRATIONS) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", v).unwrap();
        }
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    /// `(track_id, variant, path, size, duration_ms, created_at)`.
    type AudioRow = (String, String, String, Option<i64>, Option<i64>, i64);

    /// A 0001 database with a library track and its catalog twin sharing one
    /// file, a track without audio, a custom song with a copied file, and the
    /// rows that reference them.
    fn seeded_v1() -> Connection {
        let conn = db_at(1);
        conn.execute_batch(
            "INSERT INTO tracks (id, catalog_id, title, artist, audio_path, updated_at)
               VALUES ('i.lib1', '111', 'Song', 'Artist', 'media/111.m4a', 100),
                      ('111', NULL, 'Song', 'Artist', 'media/111.m4a', 101),
                      ('222', NULL, 'Other', 'Artist', NULL, 102);
             INSERT INTO tracks (id, title, artist, is_custom, audio_mode, audio_path,
                                 lyrics_source, lyrics_quality, updated_at)
               VALUES ('local-1', 'Mine', 'Me', 1, 'local', 'C:/Music/mine.flac',
                       'custom', 'line', 103);
             INSERT INTO playlists (id, kind, name, updated_at)
               VALUES ('pl.1', 'playlist', 'List', 100);
             INSERT INTO playlist_tracks (playlist_id, position, track_id)
               VALUES ('pl.1', 0, 'i.lib1'), ('pl.1', 1, '222');
             INSERT INTO downloads (id, track_id, title, artist, source_kind, status,
                                    created_at, updated_at)
               VALUES ('111', '111', 'Song', 'Artist', 'playlist', 'done', 100, 100);
             INSERT INTO download_pins (track_id, owner, created_at)
               VALUES ('111', 'pl.1', 100);
             INSERT INTO library_collections (id, kind, position, name, synced_at)
               VALUES ('p.1', 'playlist', 0, 'Synced', 100);
             INSERT INTO library_collection_tracks (collection_id, position, track_id)
               VALUES ('p.1', 0, 'i.lib1');
             INSERT INTO track_mappings (track_id, file_path, created_at)
               VALUES ('222', 'C:/Music/other.mp3', 100);
             INSERT INTO id_seq (name, next) VALUES ('song', 1);
             INSERT INTO auth_tokens (key, value, updated_at) VALUES ('developer', 'x', 100);
             INSERT INTO settings (key, value, updated_at) VALUES ('storefront', 'us', 100);
             -- Rows the new CHECKs would reject; the copy must normalise them.
             INSERT INTO tracks (id, title, artist, has_lyrics, lyrics_source, lyrics_quality,
                                 updated_at)
               VALUES ('333', 'Odd', 'Artist', 2, 'none', 'line', 104);
             INSERT INTO downloads (id, track_id, title, artist, source_kind, status,
                                    deferred, created_at, updated_at)
               VALUES ('333', '333', 'Odd', 'Artist', 'mixtape', 'weird', 5, 100, 100);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn migrate_should_bring_a_fresh_database_to_the_latest_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migrate_should_be_a_no_op_when_run_twice() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migrate_should_refuse_a_database_from_a_newer_app() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = migrate(&mut conn).unwrap_err();
        assert!(
            matches!(error, StorageError::SchemaTooNew { found, supported }
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
            "{error}"
        );
    }

    #[test]
    fn migrate_should_restore_the_foreign_keys_setting() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let on: bool = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert!(on);
    }

    #[test]
    fn v2_should_move_every_audio_path_into_a_normal_variant() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT track_id, variant, path, size, duration_ms, created_at
                 FROM track_audio ORDER BY track_id",
            )
            .unwrap();
        let rows: Vec<AudioRow> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let normal = |id: &str, path: &str, at: i64| {
            (
                id.to_owned(),
                "stereo".to_owned(),
                path.to_owned(),
                None,
                None,
                at,
            )
        };
        assert_eq!(
            rows,
            vec![
                normal("111", "media/111.m4a", 101),
                normal("local-1", "C:/Music/mine.flac", 103),
            ]
        );
    }

    #[test]
    fn v2_should_drop_the_audio_path_column() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let columns = count(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = 'audio_path'",
        );
        assert_eq!(columns, 0);
    }

    #[test]
    fn v2_should_keep_every_other_row() {
        let mut conn = seeded_v1();
        let before: Vec<i64> = TABLES
            .iter()
            .map(|t| count(&conn, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        migrate(&mut conn).unwrap();
        let after: Vec<i64> = TABLES
            .iter()
            .map(|t| count(&conn, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn v2_should_keep_one_row_per_song_and_the_other_ids_as_aliases() {
        let mut conn = seeded_v1();
        let rows = count(&conn, "SELECT COUNT(*) FROM tracks");
        migrate(&mut conn).unwrap();
        assert_eq!(
            (
                count(&conn, "SELECT COUNT(*) FROM tracks"),
                count(&conn, "SELECT COUNT(*) FROM track_aliases"),
                count(
                    &conn,
                    "SELECT COUNT(*) FROM track_aliases WHERE alias = 'i.lib1' AND track_id = '111'"
                ),
            ),
            (rows - 1, 1, 1)
        );
    }

    /// A 0001 database with one song under three ids: the catalog row
    /// `500` and library rows `i.x` / `a.500`, which carry the lyrics, the
    /// library flag, metadata the catalog row lacks, a pin under the library
    /// id, a link to a local file and a custom song's reference. `i.only` is
    /// a library row whose catalog song was never stored.
    fn seeded_v1_triplet() -> Connection {
        let conn = db_at(1);
        conn.execute_batch(
            r#"INSERT INTO tracks (id, catalog_id, title, artist, album, metadata, audio_path,
                                  lyrics_source, lyrics_quality, in_library, updated_at)
                VALUES ('500', NULL, 'Song', 'Artist', NULL, '{"id":"500"}',
                        'media/500.m4a', 'none', 'none', 0, 100),
                       ('i.x', '500', 'Song', 'Artist', 'Album', '{"id":"i.x"}',
                        'media/500.m4a', 'lrclib', 'line', 1, 101),
                       ('a.500', '500', 'Song', 'Artist', 'Album', '{"id":"a.500"}',
                        'media/500.m4a', 'custom', 'word', 0, 102),
                       ('i.only', '600', 'Other', 'Artist', NULL, NULL, 'media/600.m4a',
                        'none', 'none', 1, 103);
              INSERT INTO tracks (id, title, artist, is_custom, audio_mode, reference_track_id,
                                  updated_at)
                VALUES ('local-1', 'Mine', 'Me', 1, 'reference', 'i.x', 104);
              INSERT INTO download_pins (track_id, owner, created_at)
                VALUES ('500', 'p.1', 1), ('i.x', 'p.1', 1), ('i.x', 'p.2', 1);
              INSERT INTO track_mappings (track_id, file_path, created_at)
                VALUES ('a.500', 'C:/Music/song.mp3', 1);
              INSERT INTO library_collections (id, kind, position, name, synced_at)
                VALUES ('p.1', 'playlist', 0, 'List', 1);
              INSERT INTO library_collection_tracks (collection_id, position, track_id)
                VALUES ('p.1', 0, 'i.x'), ('p.1', 1, 'a.500');"#,
        )
        .unwrap();
        conn
    }

    fn text(conn: &Connection, sql: &str) -> String {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn v2_should_merge_twin_rows_into_their_catalog_song() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        assert_eq!(
            (
                count(&conn, "SELECT COUNT(*) FROM tracks WHERE is_custom = 0"),
                count(
                    &conn,
                    "SELECT COUNT(*) FROM track_aliases WHERE track_id = '500'"
                ),
                count(&conn, "SELECT COUNT(*) FROM track_audio"),
            ),
            (2, 2, 2)
        );
    }

    #[test]
    fn v2_should_merge_the_best_column_values_of_twins() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        assert_eq!(
            text(
                &conn,
                "SELECT album || '/' || lyrics_source || '/' || lyrics_quality || '/' || in_library
                 || '/' || metadata FROM tracks WHERE id = '500'"
            ),
            r#"Album/custom/word/1/{"id":"500"}"#
        );
    }

    #[test]
    fn v2_should_keep_each_twin_resource_under_its_alias() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        assert_eq!(
            text(
                &conn,
                "SELECT group_concat(alias || '=' || metadata || '/' || in_library, ' ')
                 FROM (SELECT * FROM track_aliases WHERE track_id = '500' ORDER BY alias)"
            ),
            r#"a.500={"id":"a.500"}/0 i.x={"id":"i.x"}/1"#
        );
    }

    #[test]
    fn v2_should_give_a_library_only_row_its_catalog_id() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        assert_eq!(
            text(
                &conn,
                "SELECT t.id || '/' || a.alias || '/' || (SELECT path FROM track_audio
                                                           WHERE track_id = t.id)
                 FROM tracks t JOIN track_aliases a ON a.track_id = t.id WHERE t.id = '600'"
            ),
            "600/i.only/media/600.m4a"
        );
    }

    #[test]
    fn v2_should_move_pins_links_and_references_to_the_song() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        assert_eq!(
            (
                text(
                    &conn,
                    "SELECT group_concat(track_id || ':' || owner, ' ')
                     FROM (SELECT * FROM download_pins ORDER BY owner)"
                ),
                text(&conn, "SELECT track_id FROM track_mappings"),
                text(
                    &conn,
                    "SELECT reference_track_id FROM tracks WHERE id = 'local-1'"
                ),
            ),
            (
                "600: 500:p.1 500:p.2".to_owned(),
                "500".to_owned(),
                "500".to_owned()
            )
        );
    }

    #[test]
    fn v2_should_keep_collection_members_as_apple_listed_them() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        assert_eq!(
            text(
                &conn,
                "SELECT group_concat(track_id, ' ') FROM
                   (SELECT track_id FROM library_collection_tracks ORDER BY position)"
            ),
            "i.x a.500"
        );
    }

    #[test]
    fn v2_triplet_should_pass_integrity_and_foreign_key_checks() {
        let mut conn = seeded_v1_triplet();
        migrate(&mut conn).unwrap();
        let integrity = text(&conn, "PRAGMA integrity_check");
        assert_eq!(
            (
                integrity.as_str(),
                count(&conn, "SELECT COUNT(*) FROM pragma_foreign_key_check")
            ),
            ("ok", 0)
        );
    }

    const TABLES: &[&str] = &[
        "playlists",
        "playlist_tracks",
        "library_collections",
        "library_collection_tracks",
        "downloads",
        "download_pins",
        "track_mappings",
        "id_seq",
        "auth_tokens",
        "settings",
    ];

    #[test]
    fn v2_should_normalise_values_the_new_checks_reject() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let track: (i64, String) = conn
            .query_row(
                "SELECT has_lyrics, lyrics_quality FROM tracks WHERE id = '333'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let download: (String, String, i64) = conn
            .query_row(
                "SELECT source_kind, status, deferred FROM downloads WHERE id = '333'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (track, download),
            (
                (1, "none".to_owned()),
                ("song".to_owned(), "failed".to_owned(), 1)
            )
        );
    }

    /// Statements each new CHECK must reject after 0002.
    const REJECTED: &[&str] = &[
        "UPDATE tracks SET is_custom = 2 WHERE id = '222'",
        "UPDATE tracks SET in_library = -1 WHERE id = '222'",
        "UPDATE tracks SET has_time_synced_lyrics = 3 WHERE id = '222'",
        "UPDATE tracks SET lyrics_quality = 'word' WHERE id = '222'",
        "UPDATE playlists SET is_custom = 7 WHERE id = 'pl.1'",
        "UPDATE downloads SET status = 'lost' WHERE id = '111'",
        "UPDATE downloads SET source_kind = 'radio' WHERE id = '111'",
        "UPDATE downloads SET deferred = 2 WHERE id = '111'",
    ];

    #[test]
    fn v2_checks_should_reject_invalid_values() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let accepted: Vec<&str> = REJECTED
            .iter()
            .copied()
            .filter(|sql| conn.execute(sql, []).is_ok())
            .collect();
        assert_eq!(accepted, Vec::<&str>::new());
    }

    #[test]
    fn v2_should_accept_lyrics_with_a_quality() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let changed = conn
            .execute(
                "UPDATE tracks SET lyrics_source = 'apple', lyrics_quality = 'word'
                 WHERE id = '222'",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    #[test]
    fn v2_should_make_the_small_tables_without_rowid() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table'
                 AND sql LIKE '%WITHOUT ROWID' ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            names,
            [
                "auth_tokens",
                "download_collections",
                "download_pins",
                "id_seq",
                "library_collection_tracks",
                "playlist_tracks",
                "song_spatial",
                "track_audio",
                "track_mappings",
            ]
        );
    }

    #[test]
    fn v2_should_leave_exactly_the_reviewed_indexes() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'index' AND sql IS NOT NULL
                 ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            names,
            [
                "artwork_files_base",
                "download_pins_owner",
                "downloads_status_idx",
                "downloads_track_idx",
                "library_collection_tracks_track",
                "playlist_tracks_track",
                "playlists_is_custom",
                "track_aliases_track",
                "tracks_is_custom",
                "tracks_real_album",
            ]
        );
    }

    #[test]
    fn v2_should_leave_no_new_tables_behind() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_schema WHERE name LIKE 'new\\_%' ESCAPE '\\'"
            ),
            0
        );
    }

    #[test]
    fn deleting_a_playlist_should_still_cascade_to_its_tracks() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        conn.execute("DELETE FROM playlists WHERE id = 'pl.1'", [])
            .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM playlist_tracks"), 0);
    }

    #[test]
    fn deleting_a_synced_collection_should_still_cascade_to_its_tracks() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        conn.execute("DELETE FROM library_collections WHERE id = 'p.1'", [])
            .unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM library_collection_tracks"),
            0
        );
    }

    #[test]
    fn v2_should_keep_custom_song_columns() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let row: (String, String, String, i64) = conn
            .query_row(
                "SELECT audio_mode, lyrics_source, lyrics_quality, is_custom
                 FROM tracks WHERE id = 'local-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "local".to_owned(),
                "custom".to_owned(),
                "line".to_owned(),
                1
            )
        );
    }

    #[test]
    fn v2_should_pass_integrity_and_foreign_key_checks() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM pragma_foreign_key_check"),
            0
        );
    }

    #[test]
    fn track_audio_should_reject_an_unknown_variant() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let result = conn.execute(
            "INSERT INTO track_audio (track_id, variant, path, created_at)
             VALUES ('222', 'hires', 'media/222.flac', 1)",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn track_audio_should_reject_a_missing_track() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let result = conn.execute(
            "INSERT INTO track_audio (track_id, variant, path, created_at)
             VALUES ('nope', 'stereo', 'media/nope.m4a', 1)",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn track_audio_should_reject_a_non_integer_size() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        let result = conn.execute(
            "INSERT INTO track_audio (track_id, variant, path, size, created_at)
             VALUES ('222', 'spatial', 'media/222.spatial.m4a', 'big', 1)",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn deleting_a_track_should_delete_its_audio_rows() {
        let mut conn = seeded_v1();
        migrate(&mut conn).unwrap();
        conn.execute("DELETE FROM tracks WHERE id = '111'", [])
            .unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM track_audio WHERE track_id = '111'"
            ),
            0
        );
    }

    #[test]
    fn a_failed_migration_should_leave_the_previous_version() {
        let mut conn = seeded_v1();
        // Make 0002 fail at its very end, after every table was rebuilt.
        conn.execute_batch("CREATE TABLE song_spatial (x)").unwrap();
        assert!(migrate(&mut conn).is_err());
        assert_eq!(current_version(&conn).unwrap(), 1);
        let columns = count(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = 'audio_path'",
        );
        assert_eq!(columns, 1);
    }

    /// A 0001 database for the download-management part of 0002: a synced
    /// playlist `p.1` downloaded with one pinned song (`111`, also linked from
    /// its library twin `i.lib1`), a song with a file but no pin (`222`), a
    /// custom song with a copied file, and a pin owned by a collection that is
    /// not in the synced library (`pl.web`).
    fn seeded_v1_downloads() -> Connection {
        let conn = db_at(1);
        conn.execute_batch(
            "INSERT INTO tracks (id, catalog_id, title, artist, audio_path, updated_at)
               VALUES ('i.lib1', '111', 'Song', 'Artist', 'media/111.m4a', 100),
                      ('111', NULL, 'Song', 'Artist', 'media/111.m4a', 100),
                      ('i.lib2', '222', 'Old', 'Artist', 'media/222.m4a', 100),
                      ('222', NULL, 'Old', 'Artist', 'media/222.m4a', 100),
                      ('333', NULL, 'Web', 'Artist', 'media/333.m4a', 100);
             INSERT INTO tracks (id, title, artist, is_custom, audio_mode, audio_path,
                                 updated_at)
               VALUES ('local-1', 'Mine', 'Me', 1, 'local', 'media/local-1.flac', 100);
             INSERT INTO library_collections (id, kind, position, name, synced_at)
               VALUES ('p.1', 'playlist', 0, 'Synced', 100);
             INSERT INTO library_collection_tracks (collection_id, position, track_id)
               VALUES ('p.1', 0, 'i.lib1');
             INSERT INTO download_pins (track_id, owner, created_at)
               VALUES ('111', 'p.1', 100), ('333', 'pl.web', 100);
             INSERT INTO downloads (id, track_id, title, artist, source_kind, status,
                                    created_at, updated_at)
               VALUES ('111', '111', 'Song', 'Artist', 'playlist', 'done', 100, 100);",
        )
        .unwrap();
        conn
    }

    fn pins(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT track_id, owner FROM download_pins ORDER BY track_id, owner")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn v2_downloads_should_register_only_pinned_library_collections() {
        let mut conn = seeded_v1_downloads();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare("SELECT owner, filter, kind, name, keep FROM download_collections")
            .unwrap();
        let rows: Vec<(String, String, String, String, bool)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [(
                "p.1".to_owned(),
                "apple_music".to_owned(),
                "playlist".to_owned(),
                "Synced".to_owned(),
                false
            )]
        );
    }

    #[test]
    fn v2_downloads_should_pin_downloaded_songs_that_have_no_pin() {
        let mut conn = seeded_v1_downloads();
        migrate(&mut conn).unwrap();
        assert_eq!(
            pins(&conn),
            [
                ("111".to_owned(), "p.1".to_owned()),
                ("222".to_owned(), String::new()),
                ("333".to_owned(), "pl.web".to_owned()),
            ]
        );
    }

    #[test]
    fn v2_downloads_should_leave_every_download_row_a_plain_download() {
        let mut conn = seeded_v1_downloads();
        migrate(&mut conn).unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM downloads WHERE replace_existing = 0"
            ),
            1
        );
    }

    #[test]
    fn v2_download_checks_should_reject_invalid_values() {
        let mut conn = seeded_v1_downloads();
        migrate(&mut conn).unwrap();
        for sql in [
            "UPDATE downloads SET replace_existing = 2",
            "INSERT INTO download_collections (owner, filter, kind, created_at)
               VALUES ('', 'all', 'album', 1)",
            "INSERT INTO download_collections (owner, filter, kind, created_at)
               VALUES ('x', 'nope', 'album', 1)",
            "INSERT INTO download_collections (owner, filter, kind, keep, created_at)
               VALUES ('x', 'all', 'album', 3, 1)",
        ] {
            assert!(conn.execute(sql, []).is_err(), "{sql}");
        }
    }

    #[test]
    fn v2_downloads_should_pass_integrity_and_foreign_key_checks() {
        let mut conn = seeded_v1_downloads();
        migrate(&mut conn).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM pragma_foreign_key_check"),
            0
        );
    }

    /// Migrates a copy of a real database and prints what changed. Point
    /// `CIDER_DB_COPY` at a COPY (never the live file) and run with
    /// `cargo test -p cider-offline-core real_db_copy -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs CIDER_DB_COPY pointing at a copy of a real store.sqlite"]
    #[expect(
        clippy::print_stdout,
        reason = "a manual check that reports what it saw"
    )]
    fn real_db_copy_should_migrate_cleanly() {
        let path = std::env::var_os("CIDER_DB_COPY").expect("CIDER_DB_COPY is not set");
        let mut conn = Connection::open(path).unwrap();
        let tables = |conn: &Connection| -> Vec<(String, i64)> {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_schema WHERE type = 'table'
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let names: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            names
                .into_iter()
                .map(|n| {
                    let c = count(conn, &format!("SELECT COUNT(*) FROM \"{n}\""));
                    (n, c)
                })
                .collect()
        };
        let version = current_version(&conn).unwrap();
        println!("version before: {version}");
        // From 0001 each song keeps one file (twin rows recorded the same one).
        let audio_before = if version < 2 {
            println!(
                "rows / songs before: {} / {}",
                count(&conn, "SELECT COUNT(*) FROM tracks"),
                count(
                    &conn,
                    "SELECT COUNT(DISTINCT COALESCE(NULLIF(catalog_id, ''), id)) FROM tracks"
                )
            );
            count(
                &conn,
                "SELECT COUNT(DISTINCT COALESCE(NULLIF(catalog_id, ''), id)) FROM tracks
                 WHERE audio_path IS NOT NULL AND audio_path <> ''",
            )
        } else {
            count(&conn, "SELECT COUNT(*) FROM track_audio")
        };
        println!("rows before: {:?}", tables(&conn));
        println!("tracks with audio before: {audio_before}");
        migrate(&mut conn).unwrap();
        println!("version after: {}", current_version(&conn).unwrap());
        println!("rows after: {:?}", tables(&conn));
        // From 0001 every song's file becomes a stereo row; later versions
        // keep rows.
        let audio_after = if version < 2 {
            count(
                &conn,
                "SELECT COUNT(*) FROM track_audio WHERE variant = 'stereo'",
            )
        } else {
            count(&conn, "SELECT COUNT(*) FROM track_audio")
        };
        println!("audio rows after: {audio_after}");
        println!(
            "download_collections after: {}, pins after: {}",
            count(&conn, "SELECT COUNT(*) FROM download_collections"),
            count(&conn, "SELECT COUNT(*) FROM download_pins")
        );
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        println!("integrity_check: {integrity}");
        let fk = count(&conn, "SELECT COUNT(*) FROM pragma_foreign_key_check");
        println!("foreign_key_check rows: {fk}");
        for (name, sql) in QUERY_PLANS {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let details: Vec<String> = stmt
                .query_map([], |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            println!("plan {name}: {}", details.join(" | "));
        }
        assert_eq!(audio_before, audio_after);
        assert_eq!(integrity, "ok");
        assert_eq!(fk, 0);
    }

    /// Hot queries whose plans the real-DB check prints.
    const QUERY_PLANS: &[(&str, &str)] = &[
        (
            "downloaded filter",
            "SELECT t.id FROM tracks t
             WHERE EXISTS (SELECT 1 FROM track_audio a WHERE a.track_id = t.id)
                OR EXISTS (SELECT 1 FROM downloads d
                           WHERE d.track_id = t.id AND d.status = 'done')
             ORDER BY t.artist, t.album, t.title, t.id",
        ),
        (
            "same title and artist",
            "SELECT id FROM tracks WHERE title = 'a' COLLATE NOCASE
             AND artist = 'b' COLLATE NOCASE AND id <> 'c' AND is_custom = 0",
        ),
        ("custom songs", "SELECT id FROM tracks WHERE is_custom = 1"),
        (
            "real album",
            "SELECT id FROM tracks WHERE real_album_id = 'x'",
        ),
        (
            "track columns",
            "SELECT t.id, (SELECT a.path FROM track_audio a
                           WHERE a.track_id = t.id AND a.variant = 'stereo')
             FROM tracks t
             WHERE t.id = COALESCE((SELECT a.track_id FROM track_aliases a
                                    WHERE a.alias = 'x'), 'x')",
        ),
        (
            "apple music songs",
            "SELECT t.id FROM tracks t
             WHERE t.id IN (SELECT COALESCE(ma.track_id, m.track_id)
                            FROM library_collection_tracks m
                            LEFT JOIN track_aliases ma ON ma.alias = m.track_id)",
        ),
        (
            "missing audio",
            "SELECT t.id FROM tracks t
             WHERE t.is_custom = 0
               AND NOT EXISTS (SELECT 1 FROM track_audio a WHERE a.track_id = t.id)",
        ),
    ];
}
