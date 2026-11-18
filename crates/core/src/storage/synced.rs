//! The API-synced Apple Music library (`library_collections` /
//! `library_collection_tracks`).
//!
//! Written only by the active library sync ([`crate::library_sync`]), which
//! fetches the library straight from the Apple Music API. Each successful sync
//! replaces the whole set in one transaction, so the stored library is always
//! exactly one complete API snapshot: never a mix of pages, never extended by
//! Cider's passively captured traffic.
//!
//! The library's Songs tab (`/v1/me/library/songs`: only the songs the user
//! added on their own, not every song of every library playlist) is stored
//! with the same snapshot, as the ordered id list under the
//! [`LIBRARY_SONGS_SETTING`] setting, written in the same transaction.

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use super::catalog::{TrackMetadata, get_setting, merge_track, set_setting};
use super::models::{CollectionKind, Playlist, TokenKind};
use super::repo::{now_unix, set_auth_token};

/// Setting holding the JSON array of the library Songs tab's track ids, in
/// Apple's `sort=dateAdded` order (see [`LibrarySnapshot::songs`]).
pub const LIBRARY_SONGS_SETTING: &str = "library_songs";
use super::{Storage, StorageError};

/// One library playlist / album as the API returned it, with its complete
/// ordered track list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedCollection {
    /// The collection (id, kind, name, artwork, raw resource JSON …).
    pub collection: Playlist,
    /// Every track id across all pages, in Apple's order.
    pub track_ids: Vec<String>,
    /// `meta.total` Apple reported for the track list, if it reported one.
    pub reported_total: Option<u32>,
}

/// A complete library snapshot from one sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibrarySnapshot {
    /// Library collections in Apple's listing order (playlists, then albums).
    pub collections: Vec<SyncedCollection>,
    /// Metadata of every member track (merged into `tracks`).
    pub tracks: Vec<TrackMetadata>,
    /// The library Songs tab (`/v1/me/library/songs`): the track ids of the
    /// songs the user added to the library themselves, in Apple's
    /// `sort=dateAdded` order (the order Cider's Songs page requests).
    pub songs: Vec<String>,
}

const SYNCED_COLUMNS: &str = "id, kind, name, artist, artwork_url, metadata, description, genre, release_date, reported_total";

fn synced_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Playlist, Option<u32>)> {
    let kind: String = row.get(1)?;
    let kind = CollectionKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("unknown collection kind {kind:?}").into(),
        )
    })?;
    Ok((
        Playlist {
            id: row.get(0)?,
            kind,
            name: row.get(2)?,
            artist: row.get(3)?,
            artwork_url: row.get(4)?,
            metadata: row.get(5)?,
            is_custom: false,
            description: row.get(6)?,
            genre: row.get(7)?,
            release_date: row.get(8)?,
        },
        row.get(9)?,
    ))
}

fn replace_library(conn: &mut Connection, snapshot: &LibrarySnapshot) -> Result<(), StorageError> {
    let tx = conn.transaction()?;
    for track in &snapshot.tracks {
        merge_track(&tx, track)?;
    }
    // Membership cascades with the collection rows.
    tx.execute("DELETE FROM library_collections", [])?;
    let now = now_unix();
    {
        let mut insert_collection = tx.prepare(
            "INSERT INTO library_collections
               (id, kind, position, name, artist, artwork_url, metadata, description, genre,
                release_date, reported_total, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        let mut insert_track = tx.prepare(
            "INSERT INTO library_collection_tracks (collection_id, position, track_id)
             VALUES (?1, ?2, ?3)",
        )?;
        for (position, synced) in (0_i64..).zip(&snapshot.collections) {
            let c = &synced.collection;
            insert_collection.execute(params![
                c.id,
                c.kind.as_str(),
                position,
                c.name,
                c.artist,
                c.artwork_url,
                c.metadata,
                c.description,
                c.genre,
                c.release_date,
                synced.reported_total,
                now,
            ])?;
            for (track_position, track_id) in (0_i64..).zip(&synced.track_ids) {
                insert_track.execute(params![c.id, track_position, track_id])?;
            }
        }
    }
    let songs = serde_json::to_string(&snapshot.songs)?;
    set_setting(&tx, LIBRARY_SONGS_SETTING, &songs)?;
    tx.commit()?;
    Ok(())
}

/// Every synced collection in stored (Apple listing) order, with its ordered
/// track ids.
pub(super) fn load_synced(conn: &Connection) -> Result<Vec<SyncedCollection>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SYNCED_COLUMNS} FROM library_collections ORDER BY position"
    ))?;
    let rows = stmt
        .query_map([], synced_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT collection_id, track_id FROM library_collection_tracks
             ORDER BY collection_id, position",
        )?;
        let pairs = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        for pair in pairs {
            let (collection, track): (String, String) = pair?;
            members.entry(collection).or_default().push(track);
        }
    }
    Ok(rows
        .into_iter()
        .map(|(collection, reported_total)| SyncedCollection {
            track_ids: members.remove(&collection.id).unwrap_or_default(),
            collection,
            reported_total,
        })
        .collect())
}

/// The stored Songs tab ids; empty before the first sync (or for a stored
/// value that is not a JSON string array).
fn load_songs(conn: &Connection) -> Result<Vec<String>, StorageError> {
    Ok(get_setting(conn, LIBRARY_SONGS_SETTING)?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default())
}

fn import_tokens(conn: &Connection, source: &std::path::Path) -> Result<usize, StorageError> {
    let source = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut copied = 0;
    for kind in [TokenKind::DeveloperToken, TokenKind::MediaUserToken] {
        let value: Option<String> = source
            .query_row(
                "SELECT value FROM auth_tokens WHERE key = ?1",
                [kind.key()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(value) = value {
            set_auth_token(conn, kind, &value)?;
            copied += 1;
        }
    }
    Ok(copied)
}

impl Storage {
    /// Atomically replaces the synced Apple Music library with `snapshot`
    /// (and merges its track metadata into `tracks`). Collections absent from
    /// the snapshot stop being library.
    ///
    /// # Errors
    ///
    /// Returns database errors; on error nothing is changed.
    pub async fn replace_library(&self, snapshot: LibrarySnapshot) -> Result<(), StorageError> {
        self.call(move |c| replace_library(c, &snapshot)).await
    }

    /// The synced library collections, in Apple's listing order, each with its
    /// complete ordered track ids.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn synced_library(&self) -> Result<Vec<SyncedCollection>, StorageError> {
        self.call(|c| load_synced(c)).await
    }

    /// The library Songs tab as last synced: track ids in Apple's
    /// `sort=dateAdded` order. Empty before the first successful sync.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn synced_library_songs(&self) -> Result<Vec<String>, StorageError> {
        self.call(|c| load_songs(c)).await
    }

    /// Copies the captured auth tokens from another store file (opened
    /// read-only) into this one; returns how many were copied. Token values are
    /// never logged.
    ///
    /// # Errors
    ///
    /// Returns database errors (e.g. the source cannot be opened).
    pub async fn import_auth_tokens(&self, source: PathBuf) -> Result<usize, StorageError> {
        self.call(move |c| import_tokens(c, &source)).await
    }

    /// A cheap fingerprint of everything the offline responder reads (synced
    /// library, tracks incl. audio state, mappings, saved storefront). It
    /// changes whenever any of them changes, so derived indexes can be
    /// rebuilt lazily.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn offline_fingerprint(&self) -> Result<String, StorageError> {
        self.call(|c| {
            Ok(c.query_row(
                "SELECT
                   (SELECT COUNT(*) || ':' || COALESCE(MAX(synced_at), 0) FROM library_collections)
                   || '|' || (SELECT COUNT(*) FROM library_collection_tracks)
                   || '|' || (SELECT COUNT(*) || ':' || COALESCE(MAX(updated_at), 0) || ':'
                              || COALESCE(SUM(audio_path IS NOT NULL), 0) FROM tracks)
                   || '|' || (SELECT COUNT(*) || ':' || COALESCE(MAX(created_at), 0) FROM track_mappings)
                   || '|' || COALESCE((SELECT updated_at FROM settings
                                       WHERE key = 'storefront_response'), 0)
                   || '|' || COALESCE((SELECT length(value) || ':' || updated_at FROM settings
                                       WHERE key = 'library_songs'), 0)",
                [],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Writes a consistent copy of the whole store to `dest` (`VACUUM INTO`),
    /// which must not exist yet. Used by headless checks that must not touch
    /// the real store.
    ///
    /// # Errors
    ///
    /// Returns database errors (e.g. `dest` exists or is not writable).
    pub async fn backup_into(&self, dest: PathBuf) -> Result<(), StorageError> {
        self.call(move |c| {
            let dest = dest.to_string_lossy().into_owned();
            c.execute("VACUUM INTO ?1", [dest])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(id: &str, kind: CollectionKind, tracks: &[&str]) -> SyncedCollection {
        SyncedCollection {
            collection: Playlist::minimal(id, kind, id),
            track_ids: tracks.iter().map(|t| (*t).to_owned()).collect(),
            reported_total: u32::try_from(tracks.len()).ok(),
        }
    }

    #[tokio::test]
    async fn replace_should_drop_collections_missing_from_new_snapshot() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .replace_library(LibrarySnapshot {
                collections: vec![
                    collection("p.a", CollectionKind::Playlist, &["1", "2"]),
                    collection("p.b", CollectionKind::Playlist, &["3"]),
                ],
                tracks: Vec::new(),
                songs: Vec::new(),
            })
            .await
            .unwrap();
        storage
            .replace_library(LibrarySnapshot {
                collections: vec![collection("p.b", CollectionKind::Playlist, &["3", "4"])],
                tracks: Vec::new(),
                songs: Vec::new(),
            })
            .await
            .unwrap();
        let synced = storage.synced_library().await.unwrap();
        assert_eq!(
            synced,
            vec![collection("p.b", CollectionKind::Playlist, &["3", "4"])]
        );
    }

    #[tokio::test]
    async fn replace_should_keep_duplicate_track_positions() {
        let storage = Storage::open_in_memory().unwrap();
        let snapshot = LibrarySnapshot {
            collections: vec![collection("p.a", CollectionKind::Playlist, &["1", "1"])],
            tracks: Vec::new(),
            songs: Vec::new(),
        };
        storage.replace_library(snapshot.clone()).await.unwrap();
        assert_eq!(
            storage.synced_library().await.unwrap(),
            snapshot.collections
        );
    }

    #[tokio::test]
    async fn replace_should_store_songs_in_order() {
        let storage = Storage::open_in_memory().unwrap();
        let songs = vec!["i.b".to_owned(), "i.a".to_owned()];
        storage
            .replace_library(LibrarySnapshot {
                songs: songs.clone(),
                ..LibrarySnapshot::default()
            })
            .await
            .unwrap();
        assert_eq!(storage.synced_library_songs().await.unwrap(), songs);
    }

    #[tokio::test]
    async fn songs_should_be_empty_before_first_sync() {
        let storage = Storage::open_in_memory().unwrap();
        assert_eq!(
            storage.synced_library_songs().await.unwrap(),
            Vec::<String>::new()
        );
    }
}
