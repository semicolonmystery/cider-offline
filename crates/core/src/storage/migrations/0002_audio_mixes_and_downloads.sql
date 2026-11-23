-- 0002: the 0.1.0 -> 0.1.1 upgrade. Downloaded audio mixes (stereo and
-- spatial), download management (re-download, cancel, collections that keep
-- their songs downloaded, unlinked downloads) and the fixes from the schema
-- review (CHECK constraints, WITHOUT ROWID tables, indexes).
--
-- Runs inside the migration runner's transaction with foreign keys off; the
-- runner checks `PRAGMA foreign_key_check` before committing. Tables whose
-- constraints or storage change are rebuilt with SQLite's documented 12-step
-- procedure (https://sqlite.org/lang_altertable.html#otheralter): create
-- `new_X`, copy, drop `X`, rename `new_X` to `X`, recreate its indexes.
-- No views or triggers existed, so none need recreating; the triggers
-- this migration adds are created at its end.
--
-- No table is STRICT on purpose: SQLite older than 3.37 (e.g. the one in
-- Python 3.8) cannot even open a database whose schema has one, which would
-- break inspecting a copy with common tools. typeof() checks stand in where
-- the type matters.
--
-- Copies normalise values the new CHECKs would reject (bools to 0/1, quality
-- 'none' without lyrics, unknown download enums), so an odd row can never
-- make the upgrade fail and lock the user out of their library.

-- ----- tracks: one row per song ---------------------------------------------
-- 0.1.0 stored one row per Apple id, so a song Apple listed under its catalog
-- id (`123`, type `songs`) and under library ids (`i.…` in the library,
-- `a.123` in a library playlist) had up to three rows, each with its own copy
-- of the audio, lyrics state and library flag. Lists showed the song once per
-- row and a change made through one id missed the others.
--
-- From 0002 `tracks` holds ONE row per song. Its id is the song's catalog id
-- when one is known (the id downloads, pins, lyrics files and media files are
-- keyed on), else the library id it was seen under, else `local-<n>` for a
-- custom song. Every other Apple id of the song is a row of `track_aliases`
-- (below) together with the resource Apple returned under that id. The old
-- `catalog_id` column is gone: a song's catalog id is its row id.
--
-- Every 0001 row is assigned to its song (`song_rows.song`): its catalog id,
-- or its own id. Columns are merged per song from its rows, best row first:
-- the catalog row itself, then a saved library row, then a row with audio,
-- then by id. Nullable columns take the first non-NULL value in that order;
-- flags are OR-ed; the lyrics state comes from the row with the best lyrics
-- (custom first, then the finest timing).
CREATE TEMP TABLE song_rows AS
SELECT t.*,
       CASE WHEN t.is_custom = 0 AND t.catalog_id IS NOT NULL AND t.catalog_id <> ''
                 AND t.catalog_id <> t.id
            THEN COALESCE(
                (SELECT NULLIF(c.catalog_id, '') FROM tracks c
                 WHERE c.id = t.catalog_id AND c.is_custom = 0 AND c.catalog_id <> c.id),
                t.catalog_id)
            ELSE t.id END AS song,
       0 AS score
FROM tracks t;
UPDATE song_rows SET score = (id = song) * 4 + (in_library <> 0) * 2
    + (audio_path IS NOT NULL AND audio_path <> '');
CREATE INDEX temp.song_rows_song ON song_rows (song, score DESC, id);
CREATE INDEX temp.song_rows_id ON song_rows (id);

-- Same columns as 0001 minus `catalog_id` and `audio_path` (moved to
-- `track_audio`), plus 0/1 checks on the flags and the lyrics rule: no lyrics
-- means no quality.
CREATE TABLE new_tracks (
    id            TEXT PRIMARY KEY NOT NULL,
    title         TEXT NOT NULL,
    artist        TEXT NOT NULL,
    album         TEXT,
    duration_ms   INTEGER,
    artwork_url   TEXT,
    -- The Apple resource whose id is `id` (the catalog `songs` resource),
    -- verbatim; NULL when the song was only seen under its other ids.
    metadata      TEXT,
    album_artist             TEXT,
    composer                 TEXT,
    genre                    TEXT,
    track_number             INTEGER,
    disc_number              INTEGER,
    release_date             TEXT,
    isrc                     TEXT,
    url                      TEXT,
    has_lyrics               INTEGER NOT NULL DEFAULT 0 CHECK (has_lyrics IN (0, 1)),
    has_time_synced_lyrics   INTEGER NOT NULL DEFAULT 0 CHECK (has_time_synced_lyrics IN (0, 1)),
    content_rating           TEXT,
    is_custom      INTEGER NOT NULL DEFAULT 0 CHECK (is_custom IN (0, 1)),
    real_album_id  TEXT,
    audio_mode          TEXT CHECK (audio_mode IS NULL OR audio_mode IN ('local', 'reference')),
    reference_track_id  TEXT,
    lyrics_source TEXT NOT NULL DEFAULT 'none' CHECK (lyrics_source IN ('none', 'apple', 'lrclib', 'amll', 'netease', 'kugou', 'musixmatch', 'community', 'custom')),
    lyrics_quality TEXT NOT NULL DEFAULT 'none'
        CHECK (lyrics_quality IN ('none', 'plain', 'line', 'word')),
    -- 1 when the song is a saved member of the user's library (under any id).
    in_library    INTEGER NOT NULL DEFAULT 0 CHECK (in_library IN (0, 1)),
    updated_at    INTEGER NOT NULL,
    CHECK (lyrics_source <> 'none' OR lyrics_quality = 'none')
);
INSERT INTO new_tracks
    (id, title, artist, album, duration_ms, artwork_url, metadata, album_artist,
     composer, genre, track_number, disc_number, release_date, isrc, url, has_lyrics,
     has_time_synced_lyrics, content_rating, is_custom, real_album_id, audio_mode,
     reference_track_id, lyrics_source, lyrics_quality, in_library, updated_at)
SELECT g.song,
       (SELECT r.title FROM song_rows r WHERE r.song = g.song
        ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.artist FROM song_rows r WHERE r.song = g.song
        ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.album FROM song_rows r WHERE r.song = g.song AND r.album IS NOT NULL
        ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.duration_ms FROM song_rows r WHERE r.song = g.song
          AND r.duration_ms IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.artwork_url FROM song_rows r WHERE r.song = g.song
          AND r.artwork_url IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.metadata FROM song_rows r WHERE r.id = g.song),
       (SELECT r.album_artist FROM song_rows r WHERE r.song = g.song
          AND r.album_artist IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.composer FROM song_rows r WHERE r.song = g.song
          AND r.composer IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.genre FROM song_rows r WHERE r.song = g.song
          AND r.genre IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.track_number FROM song_rows r WHERE r.song = g.song
          AND r.track_number IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.disc_number FROM song_rows r WHERE r.song = g.song
          AND r.disc_number IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.release_date FROM song_rows r WHERE r.song = g.song
          AND r.release_date IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.isrc FROM song_rows r WHERE r.song = g.song
          AND r.isrc IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.url FROM song_rows r WHERE r.song = g.song
          AND r.url IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       g.has_lyrics,
       g.has_time_synced_lyrics,
       (SELECT r.content_rating FROM song_rows r WHERE r.song = g.song
          AND r.content_rating IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       g.is_custom,
       (SELECT r.real_album_id FROM song_rows r WHERE r.song = g.song
          AND r.real_album_id IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.audio_mode FROM song_rows r WHERE r.song = g.song
          AND r.audio_mode IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       (SELECT r.reference_track_id FROM song_rows r WHERE r.song = g.song
          AND r.reference_track_id IS NOT NULL ORDER BY r.score DESC, r.id LIMIT 1),
       l.lyrics_source,
       CASE WHEN l.lyrics_source = 'none' THEN 'none' ELSE l.lyrics_quality END,
       g.in_library,
       g.updated_at
FROM (SELECT song,
             MAX(has_lyrics <> 0) AS has_lyrics,
             MAX(has_time_synced_lyrics <> 0) AS has_time_synced_lyrics,
             MAX(is_custom <> 0) AS is_custom,
             MAX(in_library <> 0) AS in_library,
             MAX(updated_at) AS updated_at
      FROM song_rows GROUP BY song) g
JOIN song_rows l ON l.id = (
    SELECT r.id FROM song_rows r WHERE r.song = g.song
    ORDER BY r.lyrics_source = 'custom' DESC,
             CASE WHEN r.lyrics_source = 'none' THEN 0
                  WHEN r.lyrics_quality = 'word' THEN 3
                  WHEN r.lyrics_quality = 'line' THEN 2
                  WHEN r.lyrics_quality = 'plain' THEN 1 ELSE 0 END DESC,
             r.lyrics_source <> 'none' DESC,
             r.score DESC, r.id
    LIMIT 1);

-- The other Apple ids of each song: library ids (`i.…`, `a.…`) and any
-- further twin. `metadata` is the resource Apple returned under that id,
-- verbatim (the offline responder answers library requests with it);
-- `in_library` is whether that id was seen as a saved library member.
-- An id names at most one song: `alias` is the key, and the triggers at the
-- end of this file keep aliases and song ids disjoint.
CREATE TABLE track_aliases (
    alias       TEXT PRIMARY KEY NOT NULL CHECK (alias <> ''),
    track_id    TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE ON UPDATE CASCADE,
    metadata    TEXT,
    in_library  INTEGER NOT NULL DEFAULT 0 CHECK (in_library IN (0, 1)),
    CHECK (alias <> track_id)
);
INSERT INTO track_aliases (alias, track_id, metadata, in_library)
SELECT id, song, metadata, in_library <> 0 FROM song_rows WHERE id <> song;

-- A song can have several downloaded versions, each its own file:
--   'stereo'  -> the stereo AAC stream Cider plays by default (and a custom
--                song's copied local file)
--   'spatial' -> the Dolby Atmos mix rendered for headphones (binaural)
-- `path` uses the same codec as every stored path: relative to the data
-- folder with `/` separators when inside it, absolute otherwise.
-- `size` (bytes) and `duration_ms` are optional facts about the file, NULL
-- when unknown (every row carried over from 0001).
-- WITHOUT ROWID: the key is the natural (track_id, variant) pair and rows
-- are small. ON UPDATE CASCADE follows a song whose id becomes its catalog
-- id once that is learned.
CREATE TABLE track_audio (
    track_id    TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE ON UPDATE CASCADE,
    variant     TEXT NOT NULL CHECK (variant IN ('stereo', 'spatial')),
    path        TEXT NOT NULL CHECK (typeof(path) = 'text' AND path <> ''),
    size        INTEGER CHECK (size IS NULL OR (typeof(size) = 'integer' AND size >= 0)),
    duration_ms INTEGER
        CHECK (duration_ms IS NULL OR (typeof(duration_ms) = 'integer' AND duration_ms >= 0)),
    created_at  INTEGER NOT NULL CHECK (typeof(created_at) = 'integer'), -- unix seconds
    PRIMARY KEY (track_id, variant)
) WITHOUT ROWID;

-- Every existing download becomes the song's 'stereo' variant. Twin rows
-- recorded the same file; the best row's path is kept.
INSERT INTO track_audio (track_id, variant, path, created_at)
SELECT r.song, 'stereo', r.audio_path, r.updated_at
FROM song_rows r
WHERE r.id = (SELECT x.id FROM song_rows x
              WHERE x.song = r.song AND x.audio_path IS NOT NULL AND x.audio_path <> ''
              ORDER BY x.score DESC, x.id LIMIT 1);

DROP TABLE tracks;
ALTER TABLE new_tracks RENAME TO tracks;

-- Custom songs that play another song's audio point at that song.
UPDATE tracks SET reference_track_id =
    (SELECT r.song FROM song_rows r WHERE r.id = tracks.reference_track_id)
WHERE reference_track_id IN (SELECT id FROM song_rows WHERE id <> song);

CREATE INDEX track_aliases_track ON track_aliases(track_id);
-- `is_custom` is almost always 0. A full index on it made the planner pick it
-- for `is_custom = 0` lookups (a near-full index walk instead of a scan);
-- only `is_custom = 1` is ever searched, so index just those rows.
CREATE INDEX tracks_is_custom ON tracks(is_custom) WHERE is_custom = 1;
-- Custom songs tagged into a given real album; only custom songs set it.
CREATE INDEX tracks_real_album ON tracks(real_album_id) WHERE real_album_id IS NOT NULL;
-- (0001's tracks_catalog_id is gone with its column, and tracks_in_library
-- because no query filters on `in_library`.)

-- ----- playlists ------------------------------------------------------------
CREATE TABLE new_playlists (
    id           TEXT PRIMARY KEY NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('playlist', 'album')),
    name         TEXT NOT NULL,
    artist       TEXT,
    artwork_url  TEXT,
    metadata     TEXT,
    is_custom    INTEGER NOT NULL DEFAULT 0 CHECK (is_custom IN (0, 1)),
    description  TEXT,
    genre        TEXT,
    release_date TEXT,
    updated_at   INTEGER NOT NULL
);
INSERT INTO new_playlists
    (id, kind, name, artist, artwork_url, metadata, is_custom, description, genre,
     release_date, updated_at)
SELECT id, kind, name, artist, artwork_url, metadata, is_custom <> 0, description, genre,
       release_date, updated_at
FROM playlists;

-- Membership tables: composite natural keys, small rows -> WITHOUT ROWID.
CREATE TABLE new_playlist_tracks (
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    track_id    TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
) WITHOUT ROWID;
INSERT INTO new_playlist_tracks (playlist_id, position, track_id)
SELECT playlist_id, position, track_id FROM playlist_tracks;

DROP TABLE playlist_tracks;
DROP TABLE playlists;
ALTER TABLE new_playlists RENAME TO playlists;
ALTER TABLE new_playlist_tracks RENAME TO playlist_tracks;
CREATE INDEX playlists_is_custom ON playlists(is_custom) WHERE is_custom = 1;
CREATE INDEX playlist_tracks_track ON playlist_tracks(track_id);

-- ----- library_collection_tracks -------------------------------------------
CREATE TABLE new_library_collection_tracks (
    collection_id TEXT NOT NULL REFERENCES library_collections(id) ON DELETE CASCADE,
    position      INTEGER NOT NULL,
    track_id      TEXT NOT NULL,
    PRIMARY KEY (collection_id, position)
) WITHOUT ROWID;
INSERT INTO new_library_collection_tracks (collection_id, position, track_id)
SELECT collection_id, position, track_id FROM library_collection_tracks;
DROP TABLE library_collection_tracks;
ALTER TABLE new_library_collection_tracks RENAME TO library_collection_tracks;
CREATE INDEX library_collection_tracks_track ON library_collection_tracks(track_id);

-- ----- downloads ------------------------------------------------------------
-- The enum columns get CHECKs matching `DownloadStatus` / `DownloadSourceKind`.
-- `progress` gets none: it is a float the queue computes, and a rounding
-- slip past 1.0 must not fail a write.
CREATE TABLE new_downloads (
    id           TEXT PRIMARY KEY NOT NULL,
    track_id     TEXT NOT NULL,
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    album        TEXT,
    duration_ms  INTEGER NOT NULL DEFAULT 0,
    artwork_url  TEXT,
    source_kind  TEXT NOT NULL CHECK (source_kind IN ('album', 'playlist', 'song')),
    source_id    TEXT,
    source_name  TEXT,
    status       TEXT NOT NULL
        CHECK (status IN ('queued', 'downloading', 'done', 'failed', 'paused', 'unavailable')),
    progress     REAL NOT NULL DEFAULT 0.0,
    message      TEXT,
    deferred     INTEGER NOT NULL DEFAULT 0 CHECK (deferred IN (0, 1)),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    -- 1 when the row is a re-download: every mix the download format wants
    -- is fetched again and replaces the existing file once the new one is
    -- complete. 0 (a plain download) fetches only the mixes not on disk yet.
    -- Every row carried over from 0001 is a plain download.
    replace_existing INTEGER NOT NULL DEFAULT 0 CHECK (replace_existing IN (0, 1))
);
INSERT INTO new_downloads
    (id, track_id, title, artist, album, duration_ms, artwork_url, source_kind, source_id,
     source_name, status, progress, message, deferred, created_at, updated_at)
SELECT id, track_id, title, artist, album, duration_ms, artwork_url,
       CASE WHEN source_kind IN ('album', 'playlist', 'song') THEN source_kind ELSE 'song' END,
       source_id, source_name,
       CASE WHEN status IN ('queued', 'downloading', 'done', 'failed', 'paused', 'unavailable')
            THEN status ELSE 'failed' END,
       progress, message, deferred <> 0, created_at, updated_at
FROM downloads;
DROP TABLE downloads;
ALTER TABLE new_downloads RENAME TO downloads;
-- Queue rows are keyed on the song id (normally the catalog id already).
-- A row keyed on another id of a song moves to the song; when the song has
-- a row of its own, that one is kept.
UPDATE OR IGNORE downloads SET
    id = CASE WHEN id = track_id
              THEN (SELECT r.song FROM song_rows r WHERE r.id = downloads.track_id)
              ELSE id END,
    track_id = (SELECT r.song FROM song_rows r WHERE r.id = downloads.track_id)
WHERE track_id IN (SELECT id FROM song_rows WHERE id <> song);
DELETE FROM downloads WHERE track_id IN (SELECT id FROM song_rows WHERE id <> song);
CREATE INDEX downloads_status_idx ON downloads(status);
-- The library's "Downloaded" filter looks up a done download per track; with
-- status in the key the lookup is covering (status alone was chosen before).
CREATE INDEX downloads_track_idx ON downloads(track_id, status);
-- (0001's downloads_source_idx / downloads_deferred_idx are gone: no query
-- filters downloads by source or by `deferred`.)

-- ----- download_pins --------------------------------------------------------
CREATE TABLE new_download_pins (
    track_id   TEXT NOT NULL,
    owner      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (track_id, owner)
) WITHOUT ROWID;
INSERT INTO new_download_pins (track_id, owner, created_at)
SELECT track_id, owner, created_at FROM download_pins;
DROP TABLE download_pins;
ALTER TABLE new_download_pins RENAME TO download_pins;
-- Pins are keyed on the song id; a pin on another id of a song moves to it
-- (a duplicate of a pin the song already has is dropped).
UPDATE OR IGNORE download_pins SET
    track_id = (SELECT r.song FROM song_rows r WHERE r.id = download_pins.track_id)
WHERE track_id IN (SELECT id FROM song_rows WHERE id <> song);
DELETE FROM download_pins WHERE track_id IN (SELECT id FROM song_rows WHERE id <> song);
CREATE INDEX download_pins_owner ON download_pins(owner);

-- ----- small key/value tables -> WITHOUT ROWID ------------------------------
CREATE TABLE new_track_mappings (
    track_id   TEXT PRIMARY KEY NOT NULL,
    file_path  TEXT NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO new_track_mappings (track_id, file_path, created_at)
SELECT track_id, file_path, created_at FROM track_mappings;
DROP TABLE track_mappings;
ALTER TABLE new_track_mappings RENAME TO track_mappings;
-- A link to a local file belongs to the song; the song's own link wins over
-- one made through another of its ids.
UPDATE OR IGNORE track_mappings SET
    track_id = (SELECT r.song FROM song_rows r WHERE r.id = track_mappings.track_id)
WHERE track_id IN (SELECT id FROM song_rows WHERE id <> song);
DELETE FROM track_mappings WHERE track_id IN (SELECT id FROM song_rows WHERE id <> song);

CREATE TABLE new_id_seq (
    name TEXT PRIMARY KEY NOT NULL,
    next INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO new_id_seq (name, next) SELECT name, next FROM id_seq;
DROP TABLE id_seq;
ALTER TABLE new_id_seq RENAME TO id_seq;

CREATE TABLE new_auth_tokens (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO new_auth_tokens (key, value, updated_at)
SELECT key, value, updated_at FROM auth_tokens;
DROP TABLE auth_tokens;
ALTER TABLE new_auth_tokens RENAME TO auth_tokens;

-- `settings` stays a rowid table: its values hold whole API responses
-- (e.g. `library_songs`), and WITHOUT ROWID suits only small rows.

-- ----- download_collections ---------------------------------------------------
-- Albums and playlists of the library whose download the user asked for, so
-- their pins can be checked against what the collection holds now:
--   `owner`  the pin owner, i.e. the collection id in `download_pins.owner`
--   `filter` the library view the collection was downloaded from (its member
--            list is read through that view: 'apple_music', 'downloaded',
--            'all' or 'custom')
--   `keep`   "Keep downloaded": songs added to it later are queued after a
--            library sync
-- A pin whose owner is not listed here (a link pasted in the Downloads page,
-- an individual download) is never treated as stale.
CREATE TABLE download_collections (
    owner       TEXT PRIMARY KEY NOT NULL CHECK (owner <> ''),
    filter      TEXT NOT NULL CHECK (filter IN ('apple_music', 'downloaded', 'all', 'custom')),
    kind        TEXT NOT NULL CHECK (kind IN ('album', 'playlist')),
    name        TEXT NOT NULL DEFAULT '',
    keep        INTEGER NOT NULL DEFAULT 0 CHECK (keep IN (0, 1)),
    created_at  INTEGER NOT NULL
) WITHOUT ROWID;

-- Every pinned owner that is a synced library collection was downloaded from
-- the Apple Music library view (the only view whose ids are library ids).
INSERT INTO download_collections (owner, filter, kind, name, keep, created_at)
SELECT DISTINCT p.owner, 'apple_music', c.kind, c.name, 0,
       CAST(strftime('%s', 'now') AS INTEGER)
FROM download_pins p
JOIN library_collections c ON c.id = p.owner
WHERE p.owner <> '';

-- ----- individual pins for every earlier download ---------------------------
-- Songs downloaded before this version keep the pins they have: a song that
-- an album or playlist pins stays linked to it. A song with a file and no pin
-- at all gets an individual pin (owner ''), keyed like every pin on the song
-- id, so a file whose pin was lost is kept. Custom songs never get pins; their
-- audio is the user's own.
INSERT OR IGNORE INTO download_pins (track_id, owner, created_at)
SELECT DISTINCT t.id, '', CAST(strftime('%s', 'now') AS INTEGER)
FROM track_audio a
JOIN tracks t ON t.id = a.track_id
WHERE t.is_custom = 0
  AND NOT EXISTS (SELECT 1 FROM download_pins p WHERE p.track_id = t.id);

-- ----- song_spatial -----------------------------------------------------------
-- Whether a song has a spatial (Dolby Atmos for headphones) mix, as the
-- download queue learned it from the catalog (keyed on the download key, the
-- catalog id). Songs not listed here fall back to the `audioTraits` in their
-- stored metadata, else count as unknown. "Download missing songs" uses it
-- to tell a stereo-only song that is complete from one missing its spatial
-- mix.
CREATE TABLE song_spatial (
    track_id    TEXT PRIMARY KEY NOT NULL,
    has_spatial INTEGER NOT NULL CHECK (has_spatial IN (0, 1)),
    checked_at  INTEGER NOT NULL
) WITHOUT ROWID;

-- ----- one id, one song -----------------------------------------------------
-- Song ids (`tracks.id`) and aliases (`track_aliases.alias`) share one
-- namespace: an Apple id names at most one song. Each key is unique in its
-- own table; these triggers keep the two tables disjoint, so no id can ever
-- be a song of its own and another song's alias (the twin rows of 0.1.0).
CREATE TRIGGER track_aliases_disjoint_insert BEFORE INSERT ON track_aliases
WHEN EXISTS (SELECT 1 FROM tracks WHERE id = NEW.alias)
BEGIN SELECT RAISE(ABORT, 'track alias is a song id'); END;
CREATE TRIGGER track_aliases_disjoint_update BEFORE UPDATE OF alias ON track_aliases
WHEN EXISTS (SELECT 1 FROM tracks WHERE id = NEW.alias)
BEGIN SELECT RAISE(ABORT, 'track alias is a song id'); END;
CREATE TRIGGER tracks_disjoint_insert BEFORE INSERT ON tracks
WHEN EXISTS (SELECT 1 FROM track_aliases WHERE alias = NEW.id)
BEGIN SELECT RAISE(ABORT, 'song id is a track alias'); END;
CREATE TRIGGER tracks_disjoint_update BEFORE UPDATE OF id ON tracks
WHEN EXISTS (SELECT 1 FROM track_aliases WHERE alias = NEW.id)
BEGIN SELECT RAISE(ABORT, 'song id is a track alias'); END;

DROP TABLE temp.song_rows;
