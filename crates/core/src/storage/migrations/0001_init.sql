-- Cached HTTP responses, keyed by the normalized cache key.
CREATE TABLE http_cache (
    cache_key  TEXT PRIMARY KEY NOT NULL,
    method     TEXT NOT NULL,
    url        TEXT NOT NULL,
    status     INTEGER NOT NULL,
    headers    TEXT NOT NULL,          -- JSON array of [name, value] pairs
    body       BLOB NOT NULL,
    stored_at  INTEGER NOT NULL        -- unix seconds
);

-- Tracks: one unified model for real cached songs AND user-created custom songs.
--
-- Real tracks keep the full Apple resource verbatim in `metadata` (offline synth
-- replays it). Custom songs are rows with `is_custom = 1` and an `id` of the form
-- `local-<n>` (see the `id_seq` allocator below); they carry the same rich
-- attribute set so the library-injection layer can emit them as genuine
-- Apple-shaped resources. The typed columns below are the render / edit / inject
-- surface; the complete genre list and any attributes without a column stay in
-- `metadata`.
CREATE TABLE tracks (
    id            TEXT PRIMARY KEY NOT NULL,   -- Apple catalog / library id, or `local-<n>` for custom
    catalog_id    TEXT,                        -- catalog id of a library track (`i.…` -> numeric)
    title         TEXT NOT NULL,
    artist        TEXT NOT NULL,
    album         TEXT,
    duration_ms   INTEGER,
    artwork_url   TEXT,
    metadata      TEXT,                        -- raw / synthesized Apple JSON resource, if kept

    -- Rich metadata columns (the real song attribute set; all optional so merges
    -- and inserts stay simple).
    album_artist             TEXT,
    composer                 TEXT,
    genre                    TEXT,             -- primary genre; full `genreNames` list stays in metadata
    track_number             INTEGER,          -- position within its album (1-based)
    disc_number              INTEGER,
    release_date             TEXT,             -- ISO date string, e.g. "1993-11-23"
    isrc                     TEXT,
    url                      TEXT,             -- canonical music.apple.com URL
    has_lyrics               INTEGER NOT NULL DEFAULT 0,
    has_time_synced_lyrics   INTEGER NOT NULL DEFAULT 0,
    content_rating           TEXT,             -- e.g. "explicit" / "clean"

    -- Custom-item columns (only meaningful when is_custom = 1).
    is_custom      INTEGER NOT NULL DEFAULT 0,
    real_album_id  TEXT,                        -- real album id a custom song is tagged into
                                               -- (borrows its name/artwork; injection target)
    -- Custom-song audio provenance:
    --   'local'     -> a file copied into the media dir, path in `audio_path`
    --   'reference' -> audio fetched via the downloader for `reference_track_id`
    -- NULL for real tracks and for custom songs without audio yet.
    audio_mode          TEXT CHECK (audio_mode IS NULL OR audio_mode IN ('local', 'reference')),
    reference_track_id  TEXT,                   -- referenced catalog (adam) id when audio_mode = 'reference'

    audio_path    TEXT,                         -- local audio file, if any
    lyrics_source TEXT NOT NULL DEFAULT 'none' CHECK (lyrics_source IN ('none', 'apple', 'lrclib', 'amll', 'netease', 'kugou', 'musixmatch', 'community', 'custom')),
    -- 'custom' = imported by the user; served before anything else and never
    -- overwritten (the file is `<id>.custom.ttml`, see `LyricsStore`).
    -- How finely the local lyrics are timed: none, plain (no timing), line, or
    -- word (per word / syllable). 'none' whenever lyrics_source = 'none'.
    lyrics_quality TEXT NOT NULL DEFAULT 'none'
        CHECK (lyrics_quality IN ('none', 'plain', 'line', 'word')),

    -- Library provenance: 1 only when this song was observed as a genuinely
    -- SAVED member of the user's Apple Music library (top-level `data[]` of
    -- `/v1/me/library/songs`, or a saved library album's tracks). A song seen
    -- only inside a playlist's track list, in catalog responses or in search
    -- keeps 0. Sticky: once proven 1, a later leaner capture never clears it
    -- (upserts use MAX(old, new)). Informational only: the Apple Music library
    -- view reads the API-synced `library_collections` tables, never this flag.
    in_library    INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL
);
-- So playback / lyrics requests by either id find the same row.
CREATE INDEX tracks_catalog_id ON tracks(catalog_id);
-- Cheap enumeration of "all fake songs to inject".
CREATE INDEX tracks_is_custom ON tracks(is_custom);
-- Fast selection of the saved-library track set for the Apple Music view.
CREATE INDEX tracks_in_library ON tracks(in_library);
-- Custom songs tagged into a given real album (for real-album injection).
CREATE INDEX tracks_real_album ON tracks(real_album_id);

-- Playlists and albums share one table, distinguished by kind. Custom albums /
-- playlists are rows here with `is_custom = 1` and ids `local-al-<n>` /
-- `local-pl-<n>`; they may carry a custom cover and the richer fields below.
CREATE TABLE playlists (
    id           TEXT PRIMARY KEY NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('playlist', 'album')),
    name         TEXT NOT NULL,
    artist       TEXT,
    artwork_url  TEXT,
    metadata     TEXT,

    is_custom    INTEGER NOT NULL DEFAULT 0,
    description  TEXT,
    genre        TEXT,
    release_date TEXT,

    updated_at   INTEGER NOT NULL
);
-- Cheap enumeration of "all fake collections to inject".
CREATE INDEX playlists_is_custom ON playlists(is_custom);

-- Ordered membership of a track in a playlist or album. This is the single,
-- authoritative source for custom-collection ordering: `position` is the
-- explicit, re-orderable index a UI drag-and-drop sets, and the synthesized
-- album/playlist derives its trackNumber (position + 1) and trackCount (row
-- count) from it, so the numbering actually matches what album requests report.
CREATE TABLE playlist_tracks (
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    track_id    TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
);
CREATE INDEX playlist_tracks_track ON playlist_tracks(track_id);

-- The user's real Apple Music library, exactly as the last successful active
-- sync fetched it from the Apple Music API (`/v1/me/library/playlists`,
-- `/v1/me/library/albums` and each one's `/tracks`, every page). Replaced as
-- a whole in one transaction per sync, so a collection Apple no longer returns
-- stops being library. Passive capture of Cider's traffic NEVER writes these
-- tables; it only feeds `playlists` / `playlist_tracks` for offline replay.
-- The Apple Music library view reads only these two tables (plus the `tracks`
-- rows they reference, which the sync upserts from the same API responses).
CREATE TABLE library_collections (
    id             TEXT PRIMARY KEY NOT NULL,   -- library id (`p.…` playlist, `l.…` album)
    kind           TEXT NOT NULL CHECK (kind IN ('playlist', 'album')),
    position       INTEGER NOT NULL,            -- order Apple listed it in
    name           TEXT NOT NULL,
    artist         TEXT,
    artwork_url    TEXT,
    metadata       TEXT,                        -- the library resource JSON as Apple returned it
    description    TEXT,
    genre          TEXT,
    release_date   TEXT,
    reported_total INTEGER,                     -- `meta.total` Apple reported for its tracks
    synced_at      INTEGER NOT NULL
);

-- Ordered, complete track list of each synced library collection (Apple's
-- order). A track may legitimately appear twice in a playlist.
CREATE TABLE library_collection_tracks (
    collection_id TEXT NOT NULL REFERENCES library_collections(id) ON DELETE CASCADE,
    position      INTEGER NOT NULL,
    track_id      TEXT NOT NULL,
    PRIMARY KEY (collection_id, position)
);
CREATE INDEX library_collection_tracks_track ON library_collection_tracks(track_id);

-- User-defined mapping of an Apple track id to a local audio file.
CREATE TABLE track_mappings (
    track_id   TEXT PRIMARY KEY NOT NULL,
    file_path  TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Monotonic id allocator for custom items. Each `name` ("song", "album",
-- "playlist") owns an independent counter; allocating bumps `next` and returns
-- it, so ids are never reused even after a custom item is deleted (Cider may
-- have cached the old `local-…` id). Custom songs live in `tracks` with
-- `is_custom = 1`.
CREATE TABLE id_seq (
    name TEXT PRIMARY KEY NOT NULL,
    next INTEGER NOT NULL
);

-- Captured auth material (developer token, media-user token). Never logged.
CREATE TABLE auth_tokens (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Small key/value store (e.g. last seen storefront).
CREATE TABLE settings (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Index of artwork images stored as files under `artwork/`.
CREATE TABLE artwork_files (
    cache_key    TEXT PRIMARY KEY NOT NULL,   -- normalized cache key of the image URL
    base_key     TEXT NOT NULL,               -- image path without the size segment
    file_name    TEXT NOT NULL,               -- file name inside the artwork directory
    content_type TEXT NOT NULL,
    size         INTEGER NOT NULL,            -- bytes
    pixels       INTEGER NOT NULL,            -- width * height parsed from the URL, 0 if unknown
    stored_at    INTEGER NOT NULL
);
CREATE INDEX artwork_files_base ON artwork_files(base_key);

-- Download queue rows. Kept separate from `tracks` because a queue entry
-- outlives a single download and does not imply library membership. The
-- lifecycle columns the download-control pass needs are already here:
-- `status` (queued | downloading | done | failed | paused), `progress`
-- (0.0..=1.0), `message` (error / info text) and `deferred` (auto-cache rows
-- held until playback is idle).
CREATE TABLE downloads (
    id           TEXT PRIMARY KEY NOT NULL,   -- unique row id, usually = track_id
    track_id     TEXT NOT NULL,
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    album        TEXT,
    duration_ms  INTEGER NOT NULL DEFAULT 0,
    artwork_url  TEXT,
    source_kind  TEXT NOT NULL,               -- "album" | "playlist" | "song"
    source_id    TEXT,                        -- parent album/playlist id, NULL for singles
    source_name  TEXT,                        -- parent album/playlist name
    status       TEXT NOT NULL,               -- "queued" | "downloading" | "done" | "failed" | "paused" | "unavailable"
    progress     REAL NOT NULL DEFAULT 0.0,
    message      TEXT,
    deferred     INTEGER NOT NULL DEFAULT 0,  -- auto-cache row held until playback is idle
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX downloads_status_idx ON downloads(status);
CREATE INDEX downloads_source_idx ON downloads(source_kind, source_id);
CREATE INDEX downloads_deferred_idx ON downloads(deferred, status);

-- Download pins: who asked for a track's audio. `track_id` is the download
-- key (the catalog id, as in `downloads.track_id`); `owner` is the album /
-- playlist id that requested it, or '' for an individual (single-song or
-- auto-cache) download. Removing a collection's download drops its pins and
-- deletes only the files left with no pin, so a song shared with another
-- downloaded collection, or downloaded on its own, is kept.
CREATE TABLE download_pins (
    track_id   TEXT NOT NULL,
    owner      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (track_id, owner)
);
CREATE INDEX download_pins_owner ON download_pins(owner);
