//! SQLite-backed local store with a typed, async repository API.
//!
//! `rusqlite` is synchronous, so every operation runs on Tokio's blocking
//! thread pool via [`tokio::task::spawn_blocking`]; the async worker threads
//! never touch the database. A single connection guarded by a mutex keeps
//! writes serialized, which matches SQLite's single-writer model.

mod catalog;
mod downloads;
mod library;
mod library_page;
mod migrations;
pub mod models;
mod paths;
mod repo;
mod songs;
mod synced;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};

use paths::PathCodec;

pub use catalog::{ArtworkFile, CatalogUpdate, TrackListUpdate, TrackMetadata, TrackRecord};
pub use downloads::{
    DownloadCollection, DownloadRecord, DownloadSourceKind, DownloadStatus, RemovalPreview,
};
pub(crate) use downloads::{StoredFile, UnlinkedInputs};
pub use library::{LibraryCollection, LibraryContents, LibraryFilter};
pub use library_page::{LibraryPage, LibraryQuery, LibraryTrack, TrackFilter};
pub use migrations::SCHEMA_VERSION;
pub use models::{
    AudioFile, AudioVariant, CUSTOM_ALBUM_ID_PREFIX, CUSTOM_ID_PREFIX, CUSTOM_PLAYLIST_ID_PREFIX,
    CUSTOM_SONG_ID_PREFIX, CacheEntry, CollectionKind, CustomAudio, CustomAudioMode, LyricsQuality,
    LyricsSource, NewCustomCollection, NewCustomTrack, Playlist, TokenKind, Track, TrackAudio,
    TrackMapping,
};
pub(crate) use songs::{DroppedAudio, SongRekey};
pub use synced::{LIBRARY_SONGS_SETTING, LibrarySnapshot, SyncedCollection};

/// Re-keys and merges buffered for a slow [`Storage::subscribe_rekeys`]
/// listener before it misses some (it then reconciles everything).
const REKEY_BUFFER: usize = 256;

/// Errors from the local store.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database file could not be opened or created. Carries the concrete
    /// path and the underlying SQLite primary/extended error codes so the real
    /// cause (e.g. `SQLITE_CANTOPEN`, extended code) is never hidden behind
    /// rusqlite's terse `Display`.
    #[error(
        "could not open database at {path}: {source} (sqlite code {code:?}, extended {extended})"
    )]
    Open {
        /// Database file path that failed to open.
        path: PathBuf,
        /// Primary SQLite error code, if the failure came from SQLite.
        code: Option<rusqlite::ErrorCode>,
        /// SQLite extended error code (`0` when not an `SqliteFailure`).
        extended: i32,
        /// Underlying rusqlite error.
        #[source]
        source: rusqlite::Error,
    },
    /// The database's parent directory could not be created.
    #[error("could not create data directory {path}: {source}")]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A JSON column could not be encoded or decoded.
    #[error("invalid JSON column: {0}")]
    Json(#[from] serde_json::Error),
    /// A file path is not valid UTF-8 and cannot be stored.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    /// The connection mutex was poisoned by a panicking database call.
    #[error("database connection is poisoned")]
    Poisoned,
    /// The blocking task running the operation panicked or was cancelled.
    #[error("database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The database was written by a newer version of the app.
    #[error(
        "the database has schema version {found}, but this version of the app supports up to {supported}; update the app"
    )]
    SchemaTooNew {
        /// `user_version` found in the database.
        found: u32,
        /// Newest schema version this build knows.
        supported: u32,
    },
    /// A migration left rows that reference missing parents; it was rolled back.
    #[error("migration {version} broke {violations} foreign key reference(s); rolled back")]
    ForeignKeyViolation {
        /// Schema version the failed migration would have produced.
        version: u32,
        /// Rows reported by `PRAGMA foreign_key_check`.
        violations: usize,
    },
}

/// Handle to the local store. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    /// Grouped library views, rebuilt after writes (see `library_page`).
    views: library_page::ViewCache,
    /// Stored-path codec rooted at the database's folder.
    codec: PathCodec,
    /// Songs that took another id (see [`Self::subscribe_rekeys`]).
    rekeys: tokio::sync::broadcast::Sender<SongRekey>,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage").finish_non_exhaustive()
    }
}

impl Storage {
    /// Opens (creating if needed) the database at `path` and applies pending
    /// migrations. Blocking: call from a blocking context or at startup.
    ///
    /// File paths inside the database's folder (`path`'s parent, normally the
    /// data folder) are stored relative to it with `/` separators and resolved
    /// back to absolute paths when read, so the folder can be moved as a whole.
    /// Paths elsewhere are stored absolute.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        // SQLite does not create missing parent directories; a missing data
        // directory is reported only as `SQLITE_CANTOPEN`. Create it first so a
        // first run (or any run before the data dir exists) can open the store.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| StorageError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        // Default flags, made explicit: open read/write and create if absent.
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn =
            Connection::open_with_flags(path, flags).map_err(|source| StorageError::Open {
                path: path.to_path_buf(),
                code: source.sqlite_error_code(),
                extended: source.sqlite_extended_error_code().unwrap_or(0),
                source,
            })?;
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|parent| std::path::absolute(parent).unwrap_or_else(|_| parent.to_path_buf()));
        Self::init(conn, PathCodec::new(root))
    }

    /// Opens a fresh in-memory database (tests). It has no folder, so file
    /// paths are stored exactly as given.
    ///
    /// # Errors
    ///
    /// Returns an error if migrations fail.
    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory()?, PathCodec::default())
    }

    /// [`Storage::open`] run on the blocking pool.
    ///
    /// # Errors
    ///
    /// Same as [`Storage::open`].
    pub async fn open_async(path: PathBuf) -> Result<Self, StorageError> {
        tokio::task::spawn_blocking(move || Self::open(&path)).await?
    }

    fn init(mut conn: Connection, codec: PathCodec) -> Result<Self, StorageError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL is a no-op for in-memory databases; harmless there.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // In WAL mode NORMAL never corrupts the database; a power cut can
        // only lose the last commits. FULL (the default) syncs every commit.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrations::migrate(&mut conn)?;
        library_page::register_functions(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            views: library_page::ViewCache::default(),
            codec,
            rekeys: tokio::sync::broadcast::channel(REKEY_BUFFER).0,
        })
    }

    /// Every song re-keyed to, or merged into, its catalog id from now on,
    /// sent once the change is committed. The database only moves rows; the
    /// listener moves what lives in files (see [`crate::Library`]).
    pub(crate) fn subscribe_rekeys(&self) -> tokio::sync::broadcast::Receiver<SongRekey> {
        self.rekeys.subscribe()
    }

    /// Sends committed re-keys to the listeners (none listening is fine).
    fn publish_rekeys(&self, rekeys: Vec<SongRekey>) {
        for rekey in rekeys {
            let _ = self.rekeys.send(rekey);
        }
    }

    /// Runs `f` with exclusive access to the connection on the blocking pool.
    ///
    /// This is the escape hatch for queries the typed API does not cover.
    ///
    /// # Errors
    ///
    /// Returns whatever `f` returns, or [`StorageError::Poisoned`] /
    /// [`StorageError::Task`].
    pub async fn call<F, R>(&self, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&mut Connection) -> Result<R, StorageError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| StorageError::Poisoned)?;
            f(&mut guard)
        })
        .await?
    }

    /// Current schema version (equals [`SCHEMA_VERSION`] after open).
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn schema_version(&self) -> Result<u32, StorageError> {
        self.call(|c| Ok(migrations::current_version(c)?)).await
    }

    // ----- HTTP cache -----

    /// Inserts or replaces a cached response.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn put_cache_entry(&self, entry: CacheEntry) -> Result<(), StorageError> {
        self.call(move |c| repo::put_cache_entry(c, &entry)).await
    }

    /// Looks up a cached response by normalized key.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_cache_entry(&self, key: &str) -> Result<Option<CacheEntry>, StorageError> {
        let key = key.to_owned();
        self.call(move |c| repo::get_cache_entry(c, &key)).await
    }

    /// Deletes a cached response; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_cache_entry(&self, key: &str) -> Result<bool, StorageError> {
        let key = key.to_owned();
        self.call(move |c| repo::delete_cache_entry(c, &key)).await
    }

    /// Deletes all cached responses; returns how many were removed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn clear_http_cache(&self) -> Result<usize, StorageError> {
        self.call(|c| repo::clear_http_cache(c)).await
    }

    // ----- Tracks -----

    /// Inserts or updates a track.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn upsert_track(&self, track: Track) -> Result<(), StorageError> {
        let codec = self.codec.clone();
        let rekeys = self
            .call(move |c| {
                let tx = c.transaction()?;
                let mut rekeys = Vec::new();
                repo::upsert_track(&tx, &codec, &track, &mut rekeys)?;
                tx.commit()?;
                Ok(rekeys)
            })
            .await?;
        self.publish_rekeys(rekeys);
        Ok(())
    }

    /// Whether any audio row records the stored path `stored`.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub(crate) async fn audio_path_recorded(&self, stored: &str) -> Result<bool, StorageError> {
        let stored = stored.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM track_audio WHERE path = ?1)",
                [&stored],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Records the file a merge dropped (see [`SongRekey::dropped_audio`])
    /// as song `song`'s mix again, in place of the file it kept, if the song
    /// still records that one. Returns whether the row changed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub(crate) async fn restore_dropped_audio(
        &self,
        song: &str,
        dropped: &DroppedAudio,
    ) -> Result<bool, StorageError> {
        let (song, dropped) = (song.to_owned(), dropped.clone());
        self.call(move |c| {
            Ok(c.execute(
                "UPDATE track_audio SET path = ?4, size = ?5, duration_ms = ?6
                 WHERE track_id = ?1 AND variant = ?2 AND path = ?3",
                params![
                    song,
                    dropped.variant,
                    dropped.kept,
                    dropped.dropped,
                    dropped.size,
                    dropped.duration_ms
                ],
            )? > 0)
        })
        .await
    }

    /// Fetches a track by id.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_track(&self, id: &str) -> Result<Option<Track>, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::get_track(c, &codec, &id)).await
    }

    /// Lists all tracks ordered by artist, album, title.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_tracks(&self) -> Result<Vec<Track>, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| repo::list_tracks(c, &codec)).await
    }

    /// Deletes a track; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_track(&self, id: &str) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::delete_track(c, &id)).await
    }

    /// Sets or clears (`None`) the `variant` audio file of track row `id`;
    /// returns whether the track exists.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_track_audio(
        &self,
        id: &str,
        variant: AudioVariant,
        audio_path: Option<PathBuf>,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::set_track_audio(c, &codec, &id, variant, audio_path.as_deref()))
            .await
    }

    /// The audio files recorded for track row `id` (normal first), with their
    /// size and duration when known. File existence is not checked.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn track_audio_files(&self, id: &str) -> Result<Vec<AudioFile>, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::track_audio_files(c, &codec, &id))
            .await
    }

    /// Sets a track's lyrics source; returns whether the track exists.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_track_lyrics_source(
        &self,
        id: &str,
        source: LyricsSource,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::set_track_lyrics_source(c, &id, source))
            .await
    }

    // ----- Playlists / albums -----

    /// Inserts or updates a playlist or album.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn upsert_playlist(&self, playlist: Playlist) -> Result<(), StorageError> {
        self.call(move |c| repo::upsert_playlist(c, &playlist))
            .await
    }

    /// Fetches a playlist or album by id.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_playlist(&self, id: &str) -> Result<Option<Playlist>, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::get_playlist(c, &id)).await
    }

    /// Lists playlists and/or albums ordered by name; `None` lists both.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_playlists(
        &self,
        kind: Option<CollectionKind>,
    ) -> Result<Vec<Playlist>, StorageError> {
        self.call(move |c| repo::list_playlists(c, kind)).await
    }

    /// Deletes a playlist (and its track list); returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_playlist(&self, id: &str) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::delete_playlist(c, &id)).await
    }

    /// Replaces a playlist's ordered track list atomically.
    ///
    /// # Errors
    ///
    /// Returns database errors (e.g. unknown playlist id).
    pub async fn set_playlist_tracks(
        &self,
        playlist_id: &str,
        track_ids: Vec<String>,
    ) -> Result<(), StorageError> {
        let playlist_id = playlist_id.to_owned();
        self.call(move |c| repo::set_playlist_tracks(c, &playlist_id, &track_ids))
            .await
    }

    /// Ordered track ids of a playlist.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn playlist_track_ids(&self, playlist_id: &str) -> Result<Vec<String>, StorageError> {
        let playlist_id = playlist_id.to_owned();
        self.call(move |c| repo::playlist_track_ids(c, &playlist_id))
            .await
    }

    // ----- Track mappings -----

    /// Inserts or replaces a track → local file mapping.
    ///
    /// # Errors
    ///
    /// Returns database errors or [`StorageError::NonUtf8Path`].
    pub async fn set_track_mapping(&self, mapping: TrackMapping) -> Result<(), StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| repo::set_track_mapping(c, &codec, &mapping))
            .await
    }

    /// Fetches the mapping for a track id.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_track_mapping(
        &self,
        track_id: &str,
    ) -> Result<Option<TrackMapping>, StorageError> {
        let track_id = track_id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::get_track_mapping(c, &codec, &track_id))
            .await
    }

    /// Lists all mappings ordered by track id.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_track_mappings(&self) -> Result<Vec<TrackMapping>, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| repo::list_track_mappings(c, &codec))
            .await
    }

    /// Deletes a mapping; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_track_mapping(&self, track_id: &str) -> Result<bool, StorageError> {
        let track_id = track_id.to_owned();
        self.call(move |c| repo::delete_track_mapping(c, &track_id))
            .await
    }

    // ----- Custom items (rich API) -----

    /// Creates a custom song (a `tracks` row with `is_custom = 1`); returns its
    /// `local-<n>` id.
    ///
    /// # Errors
    ///
    /// Returns database errors or [`StorageError::NonUtf8Path`].
    pub async fn create_custom_track(&self, new: NewCustomTrack) -> Result<String, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| repo::create_custom_track(c, &codec, &new))
            .await
    }

    /// Rewrites a custom song's editable fields; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn update_custom_track(
        &self,
        id: &str,
        new: NewCustomTrack,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::update_custom_track(c, &codec, &id, &new))
            .await
    }

    /// Sets a custom song's audio provenance (copied local file vs referenced
    /// catalog id); returns whether the custom song existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_custom_track_audio(
        &self,
        id: &str,
        audio: CustomAudio,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::set_custom_track_audio(c, &codec, &id, &audio))
            .await
    }

    /// Lists every custom song, ordered by allocation.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_custom_tracks(&self) -> Result<Vec<Track>, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| repo::list_custom_tracks(c, &codec))
            .await
    }

    /// Custom songs tagged into a real album, ordered by disc/track number.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn tracks_in_real_album(
        &self,
        real_album_id: &str,
    ) -> Result<Vec<Track>, StorageError> {
        let real_album_id = real_album_id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::tracks_in_real_album(c, &codec, &real_album_id))
            .await
    }

    /// Creates a custom album or playlist; returns its `local-al-<n>` /
    /// `local-pl-<n>` id.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn create_custom_collection(
        &self,
        new: NewCustomCollection,
    ) -> Result<String, StorageError> {
        self.call(move |c| repo::create_custom_collection(c, &new))
            .await
    }

    /// Rewrites a custom collection's editable fields; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn update_custom_collection(
        &self,
        id: &str,
        new: NewCustomCollection,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::update_custom_collection(c, &id, &new))
            .await
    }

    /// Lists every custom album / playlist, ordered by name.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_custom_collections(&self) -> Result<Vec<Playlist>, StorageError> {
        self.call(|c| repo::list_custom_collections(c)).await
    }

    /// Inserts or moves a track within a collection to `position` (clamped),
    /// renumbering the ordered membership. The reorder primitive for drag-and-drop.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn place_track_in_collection(
        &self,
        collection_id: &str,
        track_id: &str,
        position: usize,
    ) -> Result<(), StorageError> {
        let collection_id = collection_id.to_owned();
        let track_id = track_id.to_owned();
        self.call(move |c| repo::place_track_in_collection(c, &collection_id, &track_id, position))
            .await
    }

    /// Removes a track from a collection, renumbering the rest; returns whether
    /// it was present.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn remove_track_from_collection(
        &self,
        collection_id: &str,
        track_id: &str,
    ) -> Result<bool, StorageError> {
        let collection_id = collection_id.to_owned();
        let track_id = track_id.to_owned();
        self.call(move |c| repo::remove_track_from_collection(c, &collection_id, &track_id))
            .await
    }

    // ----- Auth tokens -----

    /// Stores a captured token. Callers must never log `value`.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_auth_token(&self, kind: TokenKind, value: String) -> Result<(), StorageError> {
        self.call(move |c| repo::set_auth_token(c, kind, &value))
            .await
    }

    /// Reads a captured token.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_auth_token(&self, kind: TokenKind) -> Result<Option<String>, StorageError> {
        self.call(move |c| repo::get_auth_token(c, kind)).await
    }

    /// Deletes a captured token; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_auth_token(&self, kind: TokenKind) -> Result<bool, StorageError> {
        self.call(move |c| repo::delete_auth_token(c, kind)).await
    }

    // ----- Download queue -----

    /// Inserts or replaces a download row.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn upsert_download(&self, record: DownloadRecord) -> Result<(), StorageError> {
        self.call(move |c| downloads::upsert(c, &record)).await
    }

    /// Lists every download row in insertion order.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_downloads(&self) -> Result<Vec<DownloadRecord>, StorageError> {
        self.call(|c| downloads::list(c)).await
    }

    /// Looks up a download row by id.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn get_download(&self, id: &str) -> Result<Option<DownloadRecord>, StorageError> {
        let id = id.to_owned();
        self.call(move |c| downloads::get(c, &id)).await
    }

    /// Updates a download row's lifecycle state; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn update_download_status(
        &self,
        id: &str,
        status: DownloadStatus,
        progress: f32,
        message: Option<String>,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| downloads::update_status(c, &id, status, progress, message.as_deref()))
            .await
    }

    /// Deletes a download row; returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_download(&self, id: &str) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| downloads::delete(c, &id)).await
    }

    /// Records that `owner` (an album / playlist id, or `None` for an
    /// individual download) wants the audio of `track_ids` (download keys,
    /// i.e. catalog ids). Idempotent.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn pin_downloads(
        &self,
        owner: Option<&str>,
        track_ids: Vec<String>,
    ) -> Result<(), StorageError> {
        let owner = owner.unwrap_or(downloads::SONG_OWNER).to_owned();
        self.call(move |c| downloads::pin(c, &owner, &track_ids).map(drop))
            .await
    }

    /// Drops `owner`'s pins on `track_ids` (`None` = the individual pin) and
    /// returns the tracks that are left with no pin at all, whose audio may
    /// therefore be deleted.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn unpin_downloads(
        &self,
        owner: Option<&str>,
        track_ids: Vec<String>,
    ) -> Result<Vec<String>, StorageError> {
        let owner = owner.unwrap_or(downloads::SONG_OWNER).to_owned();
        self.call(move |c| downloads::unpin(c, &owner, &track_ids))
            .await
    }

    /// Drops every pin of collection `owner` (its download is removed) and
    /// returns the songs left with no pin at all, whose audio may therefore
    /// be deleted: those of `track_ids` (any id of each song) and every
    /// other song the collection pinned, such as one that left it upstream.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn release_download_collection(
        &self,
        owner: &str,
        track_ids: Vec<String>,
    ) -> Result<Vec<String>, StorageError> {
        let owner = owner.to_owned();
        self.call(move |c| {
            let tx = c.transaction()?;
            let orphans = downloads::release_owner(&tx, &owner, &track_ids)?;
            tx.commit()?;
            Ok(orphans)
        })
        .await
    }

    /// Lists every stored Apple resource, one [`Track`] per Apple id: each
    /// song's own resource and each of its aliases (see
    /// [`Storage::tracks_by_any_id`]). The offline responder answers from
    /// these; lists of songs use [`Storage::list_tracks`].
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn list_resources(&self) -> Result<Vec<Track>, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| catalog::resources(c, &codec, None))
            .await
    }

    /// Drops every pin of a track (its download was removed on purpose).
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn unpin_track(&self, track_id: &str) -> Result<(), StorageError> {
        let track_id = track_id.to_owned();
        self.call(move |c| downloads::unpin_all(c, &track_id)).await
    }

    /// Owners pinning a track: collection ids, and `""` for an individual pin.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn download_owners(&self, track_id: &str) -> Result<Vec<String>, StorageError> {
        let track_id = track_id.to_owned();
        self.call(move |c| downloads::owners(c, &track_id)).await
    }

    /// Ids of every collection that has a download pinned.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn pinned_collections(&self) -> Result<Vec<String>, StorageError> {
        self.call(|c| downloads::pinned_owners(c)).await
    }

    /// Like [`Self::update_download_status`], but only when `guard` (run
    /// while the database is locked) returns `true`. The download queue's
    /// workers write through this, so a write of a worker that was paused or
    /// cancelled meanwhile can never land after the queue's own update.
    /// Returns whether the row was written.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn update_download_status_if(
        &self,
        id: &str,
        status: DownloadStatus,
        progress: f32,
        message: Option<String>,
        guard: impl FnOnce() -> bool + Send + 'static,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| {
            if !guard() {
                return Ok(false);
            }
            downloads::update_status(c, &id, status, progress, message.as_deref())
        })
        .await
    }

    /// Sets a download row's "replace existing files" and "deferred" flags;
    /// returns whether the row exists.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_download_flags(
        &self,
        id: &str,
        replace: bool,
        deferred: bool,
    ) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| downloads::set_flags(c, &id, replace, deferred))
            .await
    }

    /// Every download pin as `(track_id, owner)`; `owner` is `""` for an
    /// individual download.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn download_pins(&self) -> Result<Vec<(String, String)>, StorageError> {
        self.call(|c| downloads::all_pins(c)).await
    }

    /// Deletes the given `(track_id, owner)` pins; returns how many existed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_download_pins(
        &self,
        pins: Vec<(String, String)>,
    ) -> Result<usize, StorageError> {
        self.call(move |c| downloads::delete_pins(c, &pins)).await
    }

    /// What removing collection `owner`'s download of `track_ids` would do:
    /// which downloaded songs would lose their files and which stay because
    /// another download still needs them.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn download_removal_preview(
        &self,
        owner: &str,
        track_ids: Vec<String>,
    ) -> Result<RemovalPreview, StorageError> {
        let owner = owner.to_owned();
        self.call(move |c| downloads::removal_preview(c, &owner, &track_ids))
            .await
    }

    /// Records a collection whose download the user asked for, so its pins
    /// can later be checked against its current songs. `keep` sets "Keep
    /// downloaded"; `None` leaves it as it is.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn register_download_collection(
        &self,
        collection: DownloadCollection,
        keep: Option<bool>,
    ) -> Result<(), StorageError> {
        self.call(move |c| downloads::register_collection(c, &collection, keep))
            .await
    }

    /// Makes `collection` a download link for its songs `track_ids` (any id
    /// of each song): pins every one of them to the collection, including
    /// songs already on disk through other downloads, and records the
    /// collection (see [`Self::register_download_collection`]; `keep` as
    /// there), in one transaction. Other owners' pins are left alone, so the
    /// songs stay while any of their downloads wants them. Returns how many
    /// songs were newly pinned by the collection.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn link_download_collection(
        &self,
        collection: DownloadCollection,
        keep: Option<bool>,
        track_ids: Vec<String>,
    ) -> Result<usize, StorageError> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let added = downloads::pin(&tx, &collection.owner, &track_ids)?;
            downloads::register_collection(&tx, &collection, keep)?;
            tx.commit()?;
            Ok(added)
        })
        .await
    }

    /// Forgets a downloaded collection; returns whether it was recorded.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn unregister_download_collection(&self, owner: &str) -> Result<bool, StorageError> {
        let owner = owner.to_owned();
        self.call(move |c| downloads::unregister_collection(c, &owner))
            .await
    }

    /// Every collection whose download the user asked for.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn download_collections(&self) -> Result<Vec<DownloadCollection>, StorageError> {
        self.call(|c| downloads::collections(c)).await
    }

    /// Records whether song `track_id` (a download key, the catalog id) has a
    /// spatial mix, as a catalog lookup showed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_song_spatial(
        &self,
        track_id: &str,
        has_spatial: bool,
    ) -> Result<(), StorageError> {
        let track_id = track_id.to_owned();
        self.call(move |c| downloads::set_song_spatial(c, &track_id, has_spatial))
            .await
    }

    /// Every song whose spatial mix was looked up, by download key: whether
    /// it has one.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn song_spatial(
        &self,
    ) -> Result<std::collections::HashMap<String, bool>, StorageError> {
        self.call(|c| downloads::song_spatial(c)).await
    }

    pub(crate) async fn unlinked_inputs(&self) -> Result<UnlinkedInputs, StorageError> {
        self.call(|c| downloads::unlinked_inputs(c)).await
    }

    /// Deletes every non-custom audio row recording the stored path `path`;
    /// returns the ids of the tracks that lost it.
    pub(crate) async fn delete_audio_path(&self, path: &str) -> Result<Vec<String>, StorageError> {
        let path = path.to_owned();
        self.call(move |c| downloads::delete_audio_path(c, &path))
            .await
    }

    /// The absolute path a stored path names.
    pub(crate) fn resolve_stored_path(&self, stored: &str) -> PathBuf {
        self.codec.decode(stored)
    }

    /// Records `path` (or clears it with `None`) as the `variant` audio file
    /// of the song any of whose ids is `id`; returns how many songs matched
    /// (0 or 1).
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_audio_by_any_id(
        &self,
        id: &str,
        variant: AudioVariant,
        path: Option<PathBuf>,
    ) -> Result<usize, StorageError> {
        self.put_audio_by_any_id_inner(id, variant, path.map(|p| AudioFile::new(variant, p)))
            .await
    }

    /// Records `file` (with its size and duration) as the audio of its
    /// variant on the song any of whose ids is `id`; returns how many songs
    /// matched (0 or 1). Replaces a previous file of that variant.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn put_audio_file_by_any_id(
        &self,
        id: &str,
        file: AudioFile,
    ) -> Result<usize, StorageError> {
        self.put_audio_by_any_id_inner(id, file.variant, Some(file))
            .await
    }

    async fn put_audio_by_any_id_inner(
        &self,
        id: &str,
        variant: AudioVariant,
        file: Option<AudioFile>,
    ) -> Result<usize, StorageError> {
        let id = id.to_owned();
        let codec = self.codec.clone();
        self.call(move |c| repo::put_audio_any(c, &codec, &id, variant, file.as_ref()))
            .await
    }

    /// Whether removing downloads must leave the audio of `id` alone: it is a
    /// custom song's own audio, or a song a custom song plays by reference.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn audio_removal_exempt(&self, id: &str) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.call(move |c| downloads::audio_removal_exempt(c, &id))
            .await
    }

    /// Deletes every audio variant of the non-custom song any of whose ids
    /// is `id` (the files are the caller's); returns how many audio rows were
    /// deleted, so `0` means there was nothing to remove.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn clear_audio_by_any_id(&self, id: &str) -> Result<usize, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::clear_audio_any(c, &id)).await
    }

    /// Download keys (catalog ids) of real tracks with no recorded audio
    /// variant.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn ids_missing_audio(&self) -> Result<Vec<String>, StorageError> {
        self.call(|c| repo::ids_missing_audio(c)).await
    }

    /// The subset of `ids` naming a track with local audio (a recorded file
    /// of any variant, or a track mapping), matched by track id or catalog id. File existence
    /// is not checked.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn ids_with_local_audio(
        &self,
        ids: Vec<String>,
    ) -> Result<std::collections::HashSet<String>, StorageError> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        self.call(move |c| repo::ids_with_local_audio(c, &ids))
            .await
    }

    /// Which variants the tracks named by `ids` have locally, keyed by the
    /// requested id, for the ids that have any (normal first). A track
    /// mapping counts as the normal variant. Matched by track id or catalog
    /// id; file existence is not checked.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn local_variants(
        &self,
        ids: Vec<String>,
    ) -> Result<std::collections::HashMap<String, Vec<AudioVariant>>, StorageError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.call(move |c| repo::local_variants(c, &ids)).await
    }

    /// Sets the lyrics source and quality of every track whose id or catalog
    /// id is `id`; returns how many rows changed.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn set_lyrics_state(
        &self,
        id: &str,
        source: LyricsSource,
        quality: LyricsQuality,
    ) -> Result<usize, StorageError> {
        let id = id.to_owned();
        self.call(move |c| repo::set_lyrics_state_any(c, &id, source, quality))
            .await
    }

    /// Deletes every download row with `status`; returns the deleted ids.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn delete_downloads_by_status(
        &self,
        status: DownloadStatus,
    ) -> Result<Vec<String>, StorageError> {
        self.call(move |c| downloads::delete_by_status(c, status))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            album: Some("Album".to_owned()),
            duration_ms: Some(180_000),
            ..Track::minimal(id, format!("Song {id}"), "Artist")
        }
    }

    #[tokio::test]
    async fn open_should_migrate_to_latest_schema() {
        let storage = Storage::open_in_memory().unwrap();
        assert_eq!(storage.schema_version().await.unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn open_should_create_missing_parent_directory() {
        // Regression: the app opens the store before anything creates the data
        // dir, so a missing parent must not fail with SQLITE_CANTOPEN.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("data").join("store.sqlite");
        assert!(!path.parent().unwrap().exists());
        let storage = Storage::open(&path).expect("open should create the parent dir");
        drop(storage);
        assert!(path.exists());
    }

    #[test]
    fn reopen_should_not_reapply_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        drop(Storage::open(&path).unwrap());
        // A second open would fail on `CREATE TABLE` if migrations re-ran.
        assert!(Storage::open(&path).is_ok());
    }

    #[test]
    fn migrations_should_create_all_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for table in [
            "auth_tokens",
            "http_cache",
            "id_seq",
            "playlist_tracks",
            "playlists",
            "track_mappings",
            "tracks",
        ] {
            assert!(names.iter().any(|n| n == table), "missing table {table}");
        }
    }

    #[tokio::test]
    async fn cache_entry_should_roundtrip() {
        let storage = Storage::open_in_memory().unwrap();
        let entry = CacheEntry {
            key: "GET https://amp-api.music.apple.com/v1/me/storefront".to_owned(),
            method: "GET".to_owned(),
            url: "https://amp-api.music.apple.com/v1/me/storefront".to_owned(),
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: b"{}".to_vec(),
            stored_at: 1,
        };
        storage.put_cache_entry(entry.clone()).await.unwrap();
        assert_eq!(
            storage.get_cache_entry(&entry.key).await.unwrap(),
            Some(entry)
        );
    }

    #[tokio::test]
    async fn track_audio_should_update() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_track(track("1")).await.unwrap();
        storage
            .set_track_audio(
                "1",
                crate::storage::AudioVariant::Stereo,
                Some(PathBuf::from("song.m4a")),
            )
            .await
            .unwrap();
        let stored = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!(stored.audio.normal, Some(PathBuf::from("song.m4a")));
    }

    #[tokio::test]
    async fn playlist_tracks_should_keep_order() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_playlist(Playlist::minimal("p.1", CollectionKind::Playlist, "Mix"))
            .await
            .unwrap();
        let ids = vec!["3".to_owned(), "1".to_owned(), "2".to_owned()];
        storage
            .set_playlist_tracks("p.1", ids.clone())
            .await
            .unwrap();
        assert_eq!(storage.playlist_track_ids("p.1").await.unwrap(), ids);
    }

    #[tokio::test]
    async fn deleting_playlist_should_cascade_to_tracks() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_playlist(Playlist::minimal("a.1", CollectionKind::Album, "Album"))
            .await
            .unwrap();
        storage
            .set_playlist_tracks("a.1", vec!["1".to_owned()])
            .await
            .unwrap();
        storage.delete_playlist("a.1").await.unwrap();
        assert_eq!(
            storage.playlist_track_ids("a.1").await.unwrap(),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn auth_token_should_overwrite() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .set_auth_token(TokenKind::MediaUserToken, "a".to_owned())
            .await
            .unwrap();
        storage
            .set_auth_token(TokenKind::MediaUserToken, "b".to_owned())
            .await
            .unwrap();
        assert_eq!(
            storage
                .get_auth_token(TokenKind::MediaUserToken)
                .await
                .unwrap()
                .as_deref(),
            Some("b")
        );
    }

    #[tokio::test]
    async fn custom_ids_should_be_monotonic_across_deletes() {
        let storage = Storage::open_in_memory().unwrap();
        let first = storage
            .create_custom_track(NewCustomTrack {
                title: "One".to_owned(),
                artist: "A".to_owned(),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        assert_eq!(first, "local-1");
        storage.delete_track(&first).await.unwrap();
        let second = storage
            .create_custom_track(NewCustomTrack {
                title: "Two".to_owned(),
                artist: "A".to_owned(),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        assert_eq!(second, "local-2");
    }

    #[tokio::test]
    async fn custom_track_reference_audio_should_persist_mode() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Ref".to_owned(),
                artist: "A".to_owned(),
                audio: CustomAudio::Reference("1594405597".to_owned()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let track = storage.get_track(&id).await.unwrap().unwrap();
        assert_eq!(track.audio_mode, Some(CustomAudioMode::Reference));
        assert_eq!(track.reference_track_id.as_deref(), Some("1594405597"));
        assert!(track.audio.is_empty());
    }

    #[tokio::test]
    async fn custom_song_tagged_to_real_album_should_be_listed() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Bonus".to_owned(),
                artist: "A".to_owned(),
                real_album_id: Some("12345".to_owned()),
                track_number: Some(13),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let tagged = storage.tracks_in_real_album("12345").await.unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].id, id);
    }

    #[tokio::test]
    async fn custom_collection_should_get_prefixed_id_and_order_tracks() {
        let storage = Storage::open_in_memory().unwrap();
        let album = storage
            .create_custom_collection(NewCustomCollection {
                kind: CollectionKind::Album,
                name: "Mixtape".to_owned(),
                artist: Some("Me".to_owned()),
                artwork_url: None,
                description: Some("notes".to_owned()),
                genre: None,
                release_date: None,
                metadata: None,
            })
            .await
            .unwrap();
        assert_eq!(album, "local-al-1");

        for title in ["a", "b", "c"] {
            let song = storage
                .create_custom_track(NewCustomTrack {
                    title: title.to_owned(),
                    artist: "Me".to_owned(),
                    ..NewCustomTrack::default()
                })
                .await
                .unwrap();
            storage
                .place_track_in_collection(&album, &song, usize::MAX)
                .await
                .unwrap();
        }
        // Move the third song (local-3) to the front.
        storage
            .place_track_in_collection(&album, "local-3", 0)
            .await
            .unwrap();
        assert_eq!(
            storage.playlist_track_ids(&album).await.unwrap(),
            vec!["local-3", "local-1", "local-2"]
        );
        assert_eq!(storage.list_custom_collections().await.unwrap().len(), 1);

        assert!(
            storage
                .remove_track_from_collection(&album, "local-1")
                .await
                .unwrap()
        );
        assert_eq!(
            storage.playlist_track_ids(&album).await.unwrap(),
            vec!["local-3", "local-2"]
        );
    }

    #[tokio::test]
    async fn track_mapping_should_be_deletable() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .set_track_mapping(TrackMapping {
                track_id: "9".to_owned(),
                file_path: PathBuf::from("a.mp3"),
            })
            .await
            .unwrap();
        assert!(storage.delete_track_mapping("9").await.unwrap());
    }

    #[tokio::test]
    async fn stored_audio_path_should_be_relative_on_disk_and_absolute_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let storage = Storage::open(&data.join("store.sqlite")).unwrap();
        storage.upsert_track(track("1")).await.unwrap();
        let file = data.join("media").join("1.m4a");
        storage
            .set_track_audio(
                "1",
                crate::storage::AudioVariant::Stereo,
                Some(file.clone()),
            )
            .await
            .unwrap();
        let raw = storage
            .call(|c| {
                Ok(
                    c.query_row("SELECT path FROM track_audio WHERE track_id='1'", [], |r| {
                        r.get::<_, String>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        let read = storage.get_track("1").await.unwrap().unwrap().audio.normal;
        assert_eq!((raw.as_str(), read), ("media/1.m4a", Some(file)));
    }

    #[tokio::test]
    async fn mapping_outside_data_dir_should_stay_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("data").join("store.sqlite")).unwrap();
        let file = dir.path().join("elsewhere").join("a.flac");
        storage
            .set_track_mapping(TrackMapping {
                track_id: "9".to_owned(),
                file_path: file.clone(),
            })
            .await
            .unwrap();
        let raw = storage
            .call(|c| {
                Ok(c.query_row(
                    "SELECT file_path FROM track_mappings WHERE track_id='9'",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(raw, file.to_str().unwrap());
    }

    #[tokio::test]
    async fn moved_database_should_resolve_paths_in_new_folder() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        {
            let storage = Storage::open(&data.join("store.sqlite")).unwrap();
            storage.upsert_track(track("1")).await.unwrap();
            storage
                .set_track_audio(
                    "1",
                    crate::storage::AudioVariant::Stereo,
                    Some(data.join("media").join("1.m4a")),
                )
                .await
                .unwrap();
            // Fold the WAL into the main file before copying it.
            storage
                .call(|c| {
                    c.pragma_update(None, "journal_mode", "DELETE")?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::copy(data.join("store.sqlite"), other.join("store.sqlite")).unwrap();
        let storage = Storage::open(&other.join("store.sqlite")).unwrap();
        assert_eq!(
            storage.get_track("1").await.unwrap().unwrap().audio.normal,
            Some(other.join("media").join("1.m4a"))
        );
    }

    #[tokio::test]
    async fn rooted_mapping_should_read_back_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let storage = Storage::open(&data.join("store.sqlite")).unwrap();
        let file = data.join("media").join("m.flac");
        storage
            .set_track_mapping(TrackMapping {
                track_id: "9".to_owned(),
                file_path: file.clone(),
            })
            .await
            .unwrap();
        let mapping = storage.get_track_mapping("9").await.unwrap().unwrap();
        assert_eq!(mapping.file_path, file);
    }

    // ----- Audio variants -----

    /// A library row `i.1` and its catalog twin `1`.
    async fn twins() -> Storage {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_track(track("1")).await.unwrap();
        storage
            .upsert_track(Track {
                catalog_id: Some("1".to_owned()),
                ..track("i.1")
            })
            .await
            .unwrap();
        storage
    }

    #[tokio::test]
    async fn track_should_carry_both_variants() {
        let storage = twins().await;
        storage
            .set_track_audio("1", AudioVariant::Stereo, Some("n.m4a".into()))
            .await
            .unwrap();
        storage
            .set_track_audio("1", AudioVariant::Spatial, Some("s.m4a".into()))
            .await
            .unwrap();
        let audio = storage.get_track("1").await.unwrap().unwrap().audio;
        assert_eq!(
            audio,
            TrackAudio {
                normal: Some("n.m4a".into()),
                spatial: Some("s.m4a".into()),
            }
        );
    }

    #[tokio::test]
    async fn set_track_audio_should_report_a_missing_track() {
        let storage = Storage::open_in_memory().unwrap();
        let exists = storage
            .set_track_audio("nope", AudioVariant::Stereo, Some("n.m4a".into()))
            .await
            .unwrap();
        assert!(!exists);
    }

    #[tokio::test]
    async fn clearing_one_variant_should_keep_the_other() {
        let storage = twins().await;
        for variant in AudioVariant::ALL {
            storage
                .set_track_audio("1", variant, Some(format!("{variant:?}.m4a").into()))
                .await
                .unwrap();
        }
        storage
            .set_track_audio("1", AudioVariant::Stereo, None)
            .await
            .unwrap();
        let track = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!(
            (track.has_audio(), track.audio),
            (
                true,
                TrackAudio {
                    normal: None,
                    spatial: Some("Spatial.m4a".into()),
                }
            )
        );
    }

    #[tokio::test]
    async fn set_audio_by_any_id_should_record_the_one_song_of_both_ids() {
        let storage = twins().await;
        let matched = storage
            .set_audio_by_any_id("i.1", AudioVariant::Spatial, Some("s.m4a".into()))
            .await
            .unwrap();
        let song = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!((matched, song.audio.spatial), (1, Some("s.m4a".into())));
    }

    #[tokio::test]
    async fn clear_audio_by_any_id_should_remove_every_variant_of_every_twin() {
        let storage = twins().await;
        for variant in AudioVariant::ALL {
            storage
                .set_audio_by_any_id("1", variant, Some("x.m4a".into()))
                .await
                .unwrap();
        }
        let matched = storage.clear_audio_by_any_id("1").await.unwrap();
        let catalog = storage.get_track("1").await.unwrap().unwrap();
        let library = storage.get_track("i.1").await.unwrap().unwrap();
        assert_eq!(
            (matched, catalog.has_audio(), library.has_audio()),
            (2, false, false)
        );
    }

    #[tokio::test]
    async fn put_audio_file_should_record_size_and_duration() {
        let storage = twins().await;
        storage
            .put_audio_file_by_any_id(
                "1",
                AudioFile {
                    size: Some(4_096),
                    duration_ms: Some(180_000),
                    ..AudioFile::new(AudioVariant::Spatial, "s.m4a")
                },
            )
            .await
            .unwrap();
        storage
            .set_track_audio("1", AudioVariant::Stereo, Some("n.m4a".into()))
            .await
            .unwrap();
        let files = storage.track_audio_files("1").await.unwrap();
        assert_eq!(
            files,
            vec![
                AudioFile::new(AudioVariant::Stereo, "n.m4a"),
                AudioFile {
                    size: Some(4_096),
                    duration_ms: Some(180_000),
                    ..AudioFile::new(AudioVariant::Spatial, "s.m4a")
                },
            ]
        );
    }

    #[tokio::test]
    async fn re_recording_the_same_path_should_keep_its_size() {
        let storage = twins().await;
        let file = AudioFile {
            size: Some(10),
            ..AudioFile::new(AudioVariant::Stereo, "n.m4a")
        };
        storage.put_audio_file_by_any_id("1", file).await.unwrap();
        storage
            .set_audio_by_any_id("1", AudioVariant::Stereo, Some("n.m4a".into()))
            .await
            .unwrap();
        let files = storage.track_audio_files("1").await.unwrap();
        assert_eq!(files[0].size, Some(10));
    }

    #[tokio::test]
    async fn recording_a_new_path_should_drop_the_old_size() {
        let storage = twins().await;
        let file = AudioFile {
            size: Some(10),
            ..AudioFile::new(AudioVariant::Stereo, "n.m4a")
        };
        storage.put_audio_file_by_any_id("1", file).await.unwrap();
        storage
            .set_audio_by_any_id("1", AudioVariant::Stereo, Some("new.m4a".into()))
            .await
            .unwrap();
        let files = storage.track_audio_files("1").await.unwrap();
        assert_eq!(files, vec![AudioFile::new(AudioVariant::Stereo, "new.m4a")]);
    }

    #[tokio::test]
    async fn upsert_track_should_store_exactly_its_audio() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_track(Track {
                audio: TrackAudio {
                    normal: Some("n.m4a".into()),
                    spatial: Some("s.m4a".into()),
                },
                ..track("1")
            })
            .await
            .unwrap();
        storage
            .upsert_track(Track {
                audio: TrackAudio::normal("n.m4a"),
                ..track("1")
            })
            .await
            .unwrap();
        let audio = storage.get_track("1").await.unwrap().unwrap().audio;
        assert_eq!(audio, TrackAudio::normal("n.m4a"));
    }

    #[tokio::test]
    async fn spatial_only_track_should_count_as_downloaded() {
        let storage = twins().await;
        storage.upsert_track(track("2")).await.unwrap();
        storage
            .set_audio_by_any_id("1", AudioVariant::Spatial, Some("s.m4a".into()))
            .await
            .unwrap();
        let missing = storage.ids_missing_audio().await.unwrap();
        let local = storage
            .ids_with_local_audio(vec!["1".to_owned(), "i.1".to_owned(), "2".to_owned()])
            .await
            .unwrap();
        let downloaded: Vec<String> = storage
            .library_contents(LibraryFilter::Downloaded)
            .await
            .unwrap()
            .tracks
            .into_iter()
            .map(|t| t.id)
            .collect();
        let mut local: Vec<String> = local.into_iter().collect();
        local.sort();
        assert_eq!(
            (missing, local, downloaded),
            (
                vec!["2".to_owned()],
                vec!["1".to_owned(), "i.1".to_owned()],
                vec!["1".to_owned()]
            )
        );
    }

    #[tokio::test]
    async fn local_variants_should_report_each_variant_by_any_id() {
        let storage = twins().await;
        storage.upsert_track(track("2")).await.unwrap();
        storage.upsert_track(track("3")).await.unwrap();
        storage
            .set_audio_by_any_id("1", AudioVariant::Spatial, Some("s.m4a".into()))
            .await
            .unwrap();
        storage
            .set_track_audio("2", AudioVariant::Stereo, Some("n.m4a".into()))
            .await
            .unwrap();
        storage
            .set_track_mapping(TrackMapping {
                track_id: "9".to_owned(),
                file_path: "m.m4a".into(),
            })
            .await
            .unwrap();
        let ids = ["1", "i.1", "2", "3", "9"].map(str::to_owned).to_vec();
        let mut found: Vec<(String, Vec<AudioVariant>)> = storage
            .local_variants(ids)
            .await
            .unwrap()
            .into_iter()
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("1".to_owned(), vec![AudioVariant::Spatial]),
                ("2".to_owned(), vec![AudioVariant::Stereo]),
                ("9".to_owned(), vec![AudioVariant::Stereo]),
                ("i.1".to_owned(), vec![AudioVariant::Spatial]),
            ]
        );
    }

    #[tokio::test]
    async fn deleting_a_track_should_delete_its_audio_rows() {
        let storage = twins().await;
        storage
            .set_track_audio("1", AudioVariant::Stereo, Some("n.m4a".into()))
            .await
            .unwrap();
        storage.delete_track("1").await.unwrap();
        storage.upsert_track(track("1")).await.unwrap();
        assert_eq!(storage.track_audio_files("1").await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn custom_local_copy_should_be_its_normal_variant() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Mine".to_owned(),
                artist: "Me".to_owned(),
                audio: CustomAudio::LocalCopy("mine.flac".into()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let files = storage.track_audio_files(&id).await.unwrap();
        assert_eq!(
            files,
            vec![AudioFile::new(AudioVariant::Stereo, "mine.flac")]
        );
    }

    #[tokio::test]
    async fn custom_reference_audio_should_drop_the_local_copy() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Mine".to_owned(),
                artist: "Me".to_owned(),
                audio: CustomAudio::LocalCopy("mine.flac".into()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        storage
            .set_custom_track_audio(&id, CustomAudio::Reference("123".to_owned()))
            .await
            .unwrap();
        let track = storage.get_track(&id).await.unwrap().unwrap();
        assert_eq!(
            (track.audio_mode, track.has_audio()),
            (Some(CustomAudioMode::Reference), false)
        );
    }

    #[test]
    fn preferred_audio_should_fall_back_to_spatial() {
        let audio = TrackAudio {
            normal: None,
            spatial: Some("s.m4a".into()),
        };
        assert_eq!(audio.preferred(), Some(Path::new("s.m4a")));
    }

    #[tokio::test]
    async fn clearing_the_lyrics_source_should_clear_its_quality() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_track(track("1")).await.unwrap();
        storage
            .set_lyrics_state("1", LyricsSource::Apple, LyricsQuality::Word)
            .await
            .unwrap();
        storage
            .set_track_lyrics_source("1", LyricsSource::None)
            .await
            .unwrap();
        let track = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!(
            (track.lyrics_source, track.lyrics_quality),
            (LyricsSource::None, LyricsQuality::None)
        );
    }

    #[tokio::test]
    async fn upsert_should_drop_a_quality_without_lyrics() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_track(Track {
                lyrics_quality: LyricsQuality::Line,
                ..track("1")
            })
            .await
            .unwrap();
        let track = storage.get_track("1").await.unwrap().unwrap();
        assert_eq!(track.lyrics_quality, LyricsQuality::None);
    }
}
