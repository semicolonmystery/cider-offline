//! Rows and repository functions for the download queue table.

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::StorageError;
use super::repo::now_unix;

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
     created_at, updated_at";

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
             deferred, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
         ON CONFLICT(id) DO UPDATE SET \
            track_id = excluded.track_id, title = excluded.title, artist = excluded.artist, \
            album = excluded.album, duration_ms = excluded.duration_ms, \
            artwork_url = excluded.artwork_url, source_kind = excluded.source_kind, \
            source_id = excluded.source_id, source_name = excluded.source_name, \
            status = excluded.status, progress = excluded.progress, \
            message = excluded.message, deferred = excluded.deferred, \
            updated_at = excluded.updated_at",
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
        ],
    )?;
    Ok(())
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

pub(super) fn pin(
    conn: &Connection,
    owner: &str,
    track_ids: &[String],
) -> Result<(), StorageError> {
    let now = now_unix();
    let mut stmt = conn.prepare(
        "INSERT INTO download_pins (track_id, owner, created_at) VALUES (?1, ?2, ?3)          ON CONFLICT(track_id, owner) DO NOTHING",
    )?;
    for track_id in track_ids {
        stmt.execute(params![track_id, owner, now])?;
    }
    Ok(())
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
    for track_id in track_ids {
        delete.execute(params![owner, track_id])?;
        let left: i64 = remaining.query_row([track_id], |row| row.get(0))?;
        if left == 0 && !orphans.contains(track_id) {
            orphans.push(track_id.clone());
        }
    }
    Ok(orphans)
}

pub(super) fn unpin_all(conn: &Connection, track_id: &str) -> Result<(), StorageError> {
    conn.execute("DELETE FROM download_pins WHERE track_id = ?1", [track_id])?;
    Ok(())
}

pub(super) fn owners(conn: &Connection, track_id: &str) -> Result<Vec<String>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT owner FROM download_pins WHERE track_id = ?1 ORDER BY owner")?;
    let rows = stmt.query_map([track_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn pinned_owners(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT owner FROM download_pins WHERE owner <> '' ORDER BY owner")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
