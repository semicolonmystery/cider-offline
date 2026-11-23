//! Rows and repository functions for the download queue table.

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::StorageError;
use super::repo::now_unix;
use super::songs::SONG_OF_P1;

/// Lifecycle state of a row in the download queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DownloadStatus {
    /// Waiting for a worker.
    #[default]
    Queued,
    /// A worker is fetching the audio file.
    Downloading,
    /// The file is on disk.
    Done,
    /// The last attempt failed; see [`DownloadRecord::message`].
    Failed,
    /// Manually held.
    Paused,
    /// Not downloadable with the current downloader (today: music videos and
    /// library-only items, which Cider itself does play). Never retried
    /// automatically; see [`DownloadRecord::message`] for the reason. The UI
    /// shows it as "Can't download yet".
    Unavailable,
}

impl DownloadStatus {
    /// SQLite string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Paused => "paused",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parses the string form; `None` for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "downloading" => Some(Self::Downloading),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "paused" => Some(Self::Paused),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// Where a queued download came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DownloadSourceKind {
    /// An Apple Music album.
    Album,
    /// An Apple Music playlist.
    Playlist,
    /// A single song (not grouped under an album or playlist).
    #[default]
    Song,
}

impl DownloadSourceKind {
    /// SQLite string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Playlist => "playlist",
            Self::Song => "song",
        }
    }

    /// Parses the string form; `None` for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "album" => Some(Self::Album),
            "playlist" => Some(Self::Playlist),
            "song" => Some(Self::Song),
            _ => None,
        }
    }
}

/// One row in the `downloads` table.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadRecord {
    /// Unique row id (usually the track id).
    pub id: String,
    /// Apple Music track id.
    pub track_id: String,
    /// Track title.
    pub title: String,
    /// Primary artist.
    pub artist: String,
    /// Album name, if known.
    pub album: Option<String>,
    /// Duration in milliseconds (0 if unknown).
    pub duration_ms: u32,
    /// Artwork URL template.
    pub artwork_url: Option<String>,
    /// Kind of parent (album / playlist / single song).
    pub source_kind: DownloadSourceKind,
    /// Parent album or playlist id, if any.
    pub source_id: Option<String>,
    /// Parent album or playlist display name, if any.
    pub source_name: Option<String>,
    /// Current lifecycle state.
    pub status: DownloadStatus,
    /// Fractional progress in `0.0..=1.0`.
    pub progress: f32,
    /// Error or informational message.
    pub message: Option<String>,
    /// True when this is an auto-cache row that must wait for an idle
    /// playback window before a worker picks it up.
    pub deferred: bool,
    /// A re-download: fetch every mix the download format wants again and
    /// replace the existing files once the new ones are complete. A plain
    /// download (`false`) fetches only the mixes that are not on disk yet.
    pub replace: bool,
    /// Unix seconds when the row was created.
    pub created_at: i64,
    /// Unix seconds of the last update.
    pub updated_at: i64,
}

fn conversion_error(column: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, message.into())
}

const COLUMNS: &str = "id, track_id, title, artist, album, duration_ms, artwork_url, \
     source_kind, source_id, source_name, status, progress, message, deferred, \
     created_at, updated_at, replace_existing";

fn row_from(row: &Row<'_>) -> rusqlite::Result<DownloadRecord> {
    let kind: String = row.get(7)?;
    let status: String = row.get(10)?;
    Ok(DownloadRecord {
        id: row.get(0)?,
        track_id: row.get(1)?,
        title: row.get(2)?,
        artist: row.get(3)?,
        album: row.get(4)?,
        duration_ms: row.get(5)?,
        artwork_url: row.get(6)?,
        source_kind: DownloadSourceKind::parse(&kind)
            .ok_or_else(|| conversion_error(7, format!("unknown source kind {kind:?}")))?,
        source_id: row.get(8)?,
        source_name: row.get(9)?,
        status: DownloadStatus::parse(&status)
            .ok_or_else(|| conversion_error(10, format!("unknown status {status:?}")))?,
        progress: row.get(11)?,
        message: row.get(12)?,
        deferred: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
        replace: row.get(16)?,
    })
}

pub(super) fn upsert(conn: &Connection, record: &DownloadRecord) -> Result<(), StorageError> {
    let now = now_unix();
    let created_at = if record.created_at == 0 {
        now
    } else {
        record.created_at
    };
    conn.execute(
        "INSERT INTO downloads \
            (id, track_id, title, artist, album, duration_ms, artwork_url, \
             source_kind, source_id, source_name, status, progress, message, \
             deferred, created_at, updated_at, replace_existing) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) \
         ON CONFLICT(id) DO UPDATE SET \
            track_id = excluded.track_id, title = excluded.title, artist = excluded.artist, \
            album = excluded.album, duration_ms = excluded.duration_ms, \
            artwork_url = excluded.artwork_url, source_kind = excluded.source_kind, \
            source_id = excluded.source_id, source_name = excluded.source_name, \
            status = excluded.status, progress = excluded.progress, \
            message = excluded.message, deferred = excluded.deferred, \
            updated_at = excluded.updated_at, replace_existing = excluded.replace_existing",
        params![
            record.id,
            record.track_id,
            record.title,
            record.artist,
            record.album,
            record.duration_ms,
            record.artwork_url,
            record.source_kind.as_str(),
            record.source_id,
            record.source_name,
            record.status.as_str(),
            record.progress,
            record.message,
            record.deferred,
            created_at,
            now,
            record.replace,
        ],
    )?;
    Ok(())
}

/// Sets a row's `replace_existing` and `deferred` flags; returns whether the
/// row exists.
pub(super) fn set_flags(
    conn: &Connection,
    id: &str,
    replace: bool,
    deferred: bool,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE downloads SET replace_existing = ?2, deferred = ?3, updated_at = ?4 \
         WHERE id = ?1",
        params![id, replace, deferred, now_unix()],
    )? > 0)
}

pub(super) fn list(conn: &Connection) -> Result<Vec<DownloadRecord>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM downloads ORDER BY created_at, id"
    ))?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<DownloadRecord>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM downloads WHERE id = ?1"),
            [id],
            row_from,
        )
        .optional()?)
}

pub(super) fn update_status(
    conn: &Connection,
    id: &str,
    status: DownloadStatus,
    progress: f32,
    message: Option<&str>,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE downloads SET status = ?2, progress = ?3, message = ?4, updated_at = ?5 \
         WHERE id = ?1",
        params![id, status.as_str(), progress, message, now_unix()],
    )? > 0)
}

pub(super) fn delete(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM downloads WHERE id = ?1", [id])? > 0)
}

pub(super) fn delete_by_status(
    conn: &Connection,
    status: DownloadStatus,
) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare("SELECT id FROM downloads WHERE status = ?1")?;
    let ids: Vec<String> = stmt
        .query_map([status.as_str()], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    conn.execute("DELETE FROM downloads WHERE status = ?1", [status.as_str()])?;
    Ok(ids)
}

// ----- Download pins -----
//
// A pin records *who* asked for a track's audio: a collection (album /
// playlist id) or the track itself (`owner = ''`, an individual download,
// including auto-cache). A file is kept while at least one pin remains.

/// Owner value of an individual (single-song) pin.
pub(super) const SONG_OWNER: &str = "";

/// Pins `track_ids` to `owner`; returns how many pins were new.
pub(super) fn pin(
    conn: &Connection,
    owner: &str,
    track_ids: &[String],
) -> Result<usize, StorageError> {
    let now = now_unix();
    // Pins are keyed on the song id, whichever id of the song is given.
    let mut stmt = conn.prepare(&format!(
        "INSERT INTO download_pins (track_id, owner, created_at) VALUES ({SONG_OF_P1}, ?2, ?3)
         ON CONFLICT(track_id, owner) DO NOTHING"
    ))?;
    let mut added = 0;
    for track_id in track_ids {
        added += stmt.execute(params![track_id, owner, now])?;
    }
    Ok(added)
}

/// Removes `owner`'s pins on `track_ids` and returns the tracks that have no
/// pin left (their files may be deleted), in input order.
pub(super) fn unpin(
    conn: &Connection,
    owner: &str,
    track_ids: &[String],
) -> Result<Vec<String>, StorageError> {
    let mut delete =
        conn.prepare("DELETE FROM download_pins WHERE owner = ?1 AND track_id = ?2")?;
    let mut remaining = conn.prepare("SELECT COUNT(*) FROM download_pins WHERE track_id = ?1")?;
    let mut orphans = Vec::new();
    for id in track_ids {
        // Pins are keyed on the song id, whichever id of the song is given.
        let track_id = super::songs::song_of(conn, id)?.unwrap_or_else(|| id.clone());
        delete.execute(params![owner, track_id])?;
        let left: i64 = remaining.query_row([&track_id], |row| row.get(0))?;
        if left == 0 && !orphans.contains(&track_id) {
            orphans.push(track_id);
        }
    }
    Ok(orphans)
}

/// `track_ids` resolved to song ids (unknown ids as given) followed by every
/// other song `owner` pins, without duplicates: what removing `owner`'s
/// download covers. A pin the collection's current songs no longer name
/// (the song left it, or was pinned under another id) is included, so no
/// pin of a removed collection is left behind.
fn owner_keys(
    conn: &Connection,
    owner: &str,
    track_ids: &[String],
) -> Result<Vec<String>, StorageError> {
    let mut keys: Vec<String> = Vec::with_capacity(track_ids.len());
    for id in track_ids {
        let key = super::songs::song_of(conn, id)?.unwrap_or_else(|| id.clone());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    let mut stmt =
        conn.prepare("SELECT track_id FROM download_pins WHERE owner = ?1 ORDER BY track_id")?;
    for key in stmt.query_map([owner], |row| row.get::<_, String>(0))? {
        let key = key?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// Removes every pin of collection `owner` and returns the songs of
/// [`owner_keys`] left with no pin (their files may be deleted), in order.
pub(super) fn release_owner(
    conn: &Connection,
    owner: &str,
    track_ids: &[String],
) -> Result<Vec<String>, StorageError> {
    let keys = owner_keys(conn, owner, track_ids)?;
    conn.execute("DELETE FROM download_pins WHERE owner = ?1", [owner])?;
    let mut remaining = conn.prepare("SELECT COUNT(*) FROM download_pins WHERE track_id = ?1")?;
    let mut orphans = Vec::new();
    for key in keys {
        let left: i64 = remaining.query_row([&key], |row| row.get(0))?;
        if left == 0 {
            orphans.push(key);
        }
    }
    Ok(orphans)
}

pub(super) fn unpin_all(conn: &Connection, track_id: &str) -> Result<(), StorageError> {
    conn.execute(
        &format!("DELETE FROM download_pins WHERE track_id = {SONG_OF_P1}"),
        [track_id],
    )?;
    Ok(())
}

pub(super) fn owners(conn: &Connection, track_id: &str) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT owner FROM download_pins WHERE track_id = {SONG_OF_P1} ORDER BY owner"
    ))?;
    let rows = stmt.query_map([track_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn pinned_owners(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT owner FROM download_pins WHERE owner <> '' ORDER BY owner")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records whether song `track_id` (a download key) has a spatial mix.
pub(super) fn set_song_spatial(
    conn: &Connection,
    track_id: &str,
    has_spatial: bool,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO song_spatial (track_id, has_spatial, checked_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(track_id) DO UPDATE SET has_spatial = excluded.has_spatial, \
            checked_at = excluded.checked_at",
        params![track_id, has_spatial, now_unix()],
    )?;
    Ok(())
}

/// Every song whose spatial mix the queue looked up, by download key.
pub(super) fn song_spatial(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, bool>, StorageError> {
    let mut stmt = conn.prepare("SELECT track_id, has_spatial FROM song_spatial")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every pin as `(track_id, owner)`.
pub(super) fn all_pins(conn: &Connection) -> Result<Vec<(String, String)>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT track_id, owner FROM download_pins ORDER BY track_id, owner")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Deletes the given `(track_id, owner)` pins; returns how many existed.
pub(super) fn delete_pins(
    conn: &Connection,
    pins: &[(String, String)],
) -> Result<usize, StorageError> {
    let mut stmt = conn.prepare("DELETE FROM download_pins WHERE track_id = ?1 AND owner = ?2")?;
    let mut deleted = 0;
    for (track_id, owner) in pins {
        deleted += stmt.execute(params![track_id, owner])?;
    }
    Ok(deleted)
}

/// Whether removing downloads must leave the audio of `id` (a download key
/// or track id) alone: it is a custom song's own audio, or a song a custom
/// song plays by reference. Custom songs have no pins, so an unpin always
/// reports them as orphans.
pub(super) fn audio_removal_exempt(conn: &Connection, id: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM tracks WHERE id = {SONG_OF_P1} AND is_custom = 1)
                 OR EXISTS (SELECT 1 FROM tracks c
                            WHERE c.is_custom = 1 AND c.reference_track_id IS NOT NULL
                              AND COALESCE((SELECT r.track_id FROM track_aliases r
                                            WHERE r.alias = c.reference_track_id),
                                           c.reference_track_id) = {SONG_OF_P1})"
        ),
        [id],
        |row| row.get(0),
    )?)
}

/// What removing a collection's download would do, listing only songs that
/// have downloaded audio (by download key).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovalPreview {
    /// Songs that would lose their files.
    pub deleted: Vec<String>,
    /// Songs whose files stay: another pin still needs them, or a custom
    /// song plays them by reference.
    pub kept: Vec<String>,
}

/// What removing `owner`'s pins on `track_ids` would do (see
/// [`RemovalPreview`]).
pub(super) fn removal_preview(
    conn: &Connection,
    owner: &str,
    track_ids: &[String],
) -> Result<RemovalPreview, StorageError> {
    let mut has_audio = conn.prepare(&format!(
        "SELECT EXISTS (SELECT 1 FROM track_audio a JOIN tracks t ON t.id = a.track_id
         WHERE t.id = {SONG_OF_P1} AND t.is_custom = 0)"
    ))?;
    let mut others = conn.prepare(
        "SELECT EXISTS (SELECT 1 FROM download_pins WHERE track_id = ?1 AND owner <> ?2)",
    )?;
    let mut preview = RemovalPreview::default();
    for track_id in owner_keys(conn, owner, track_ids)? {
        let audio: bool = has_audio.query_row([&track_id], |row| row.get(0))?;
        if !audio {
            continue;
        }
        let shared: bool = others.query_row(params![track_id, owner], |row| row.get(0))?;
        if shared || audio_removal_exempt(conn, &track_id)? {
            preview.kept.push(track_id);
        } else {
            preview.deleted.push(track_id);
        }
    }
    Ok(preview)
}

/// A library collection whose download the user asked for (see
/// `download_collections` in migration 0002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadCollection {
    /// Collection id, the owner of its pins.
    pub owner: String,
    /// Library view the collection was downloaded from.
    pub filter: super::LibraryFilter,
    /// Album or playlist.
    pub kind: DownloadSourceKind,
    /// Display name when it was last downloaded.
    pub name: String,
    /// "Keep downloaded": songs added later are queued after a library sync.
    pub keep: bool,
}

/// Records (or refreshes) a downloaded collection; `keep = None` leaves the
/// stored "Keep downloaded" flag as it is (off for a new row).
pub(super) fn register_collection(
    conn: &Connection,
    collection: &DownloadCollection,
    keep: Option<bool>,
) -> Result<(), StorageError> {
    let kind = match collection.kind {
        DownloadSourceKind::Playlist => "playlist",
        DownloadSourceKind::Album | DownloadSourceKind::Song => "album",
    };
    conn.execute(
        "INSERT INTO download_collections (owner, filter, kind, name, keep, created_at) \
         VALUES (?1, ?2, ?3, ?4, COALESCE(?5, 0), ?6) \
         ON CONFLICT(owner) DO UPDATE SET filter = excluded.filter, kind = excluded.kind, \
            name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE name END, \
            keep = COALESCE(?5, keep)",
        params![
            collection.owner,
            collection.filter.as_str(),
            kind,
            collection.name,
            keep,
            now_unix()
        ],
    )?;
    Ok(())
}

/// Forgets a downloaded collection (its download was removed).
pub(super) fn unregister_collection(conn: &Connection, owner: &str) -> Result<bool, StorageError> {
    Ok(conn.execute("DELETE FROM download_collections WHERE owner = ?1", [owner])? > 0)
}

/// Every downloaded collection, by owner.
pub(super) fn collections(conn: &Connection) -> Result<Vec<DownloadCollection>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT owner, filter, kind, name, keep FROM download_collections ORDER BY owner",
    )?;
    let rows = stmt.query_map([], |row| {
        let filter: String = row.get(1)?;
        let kind: String = row.get(2)?;
        Ok(DownloadCollection {
            owner: row.get(0)?,
            filter: super::LibraryFilter::parse(&filter)
                .ok_or_else(|| conversion_error(1, format!("unknown filter {filter:?}")))?,
            kind: if kind == "playlist" {
                DownloadSourceKind::Playlist
            } else {
                DownloadSourceKind::Album
            },
            name: row.get(3)?,
            keep: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One downloaded file as stored: the raw `track_audio.path` and the song
/// that records it (its id is the download key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredFile {
    pub path: String,
    pub track_id: String,
    pub size: Option<u64>,
}

/// What deciding which downloads are unlinked needs from the store.
#[derive(Debug, Default)]
pub(crate) struct UnlinkedInputs {
    /// Every audio row of a non-custom track.
    pub files: Vec<StoredFile>,
    /// Ids that must never lose their audio: songs a custom song plays by
    /// reference, and songs with a queue row that is not finished.
    pub protected: std::collections::HashSet<String>,
    /// Stored paths a custom song uses as its own audio.
    pub custom_paths: std::collections::HashSet<String>,
    /// The song id of every alias (another Apple id of a song), by alias.
    pub song_of: std::collections::HashMap<String, String>,
    /// Whether the Apple Music library was ever synced (it has collections).
    pub library_synced: bool,
}

pub(super) fn unlinked_inputs(conn: &Connection) -> Result<UnlinkedInputs, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT a.path, t.id, a.size FROM track_audio a \
         JOIN tracks t ON t.id = a.track_id WHERE t.is_custom = 0 ORDER BY a.path, t.id",
    )?;
    let files = stmt
        .query_map([], |row| {
            Ok(StoredFile {
                path: row.get(0)?,
                track_id: row.get(1)?,
                size: row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|size| u64::try_from(size).ok()),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut protected = std::collections::HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT COALESCE((SELECT r.track_id FROM track_aliases r \
                          WHERE r.alias = c.reference_track_id), c.reference_track_id) \
         FROM tracks c WHERE c.is_custom = 1 AND c.reference_track_id IS NOT NULL \
         UNION SELECT track_id FROM downloads WHERE status <> 'done' \
         UNION SELECT id FROM downloads WHERE status <> 'done'",
    )?;
    for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
        protected.insert(id?);
    }
    let mut stmt = conn.prepare(
        "SELECT a.path FROM track_audio a JOIN tracks t ON t.id = a.track_id WHERE t.is_custom = 1",
    )?;
    let custom_paths = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare("SELECT alias, track_id FROM track_aliases")?;
    let song_of = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let library_synced: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM library_collections)",
        [],
        |row| row.get(0),
    )?;
    Ok(UnlinkedInputs {
        files,
        protected,
        custom_paths,
        song_of,
        library_synced,
    })
}

/// Deletes every audio row that records `path` (a stored path); returns the
/// ids of the tracks that lost it.
pub(super) fn delete_audio_path(
    conn: &Connection,
    path: &str,
) -> Result<Vec<String>, StorageError> {
    const NOT_CUSTOM: &str =
        "path = ?1 AND track_id IN (SELECT id FROM tracks WHERE is_custom = 0)";
    let mut stmt = conn.prepare(&format!(
        "SELECT track_id FROM track_audio WHERE {NOT_CUSTOM}"
    ))?;
    let ids: Vec<String> = stmt
        .query_map([path], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    conn.execute(
        &format!("DELETE FROM track_audio WHERE {NOT_CUSTOM}"),
        [path],
    )?;
    Ok(ids)
}
