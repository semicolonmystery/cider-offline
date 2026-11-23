//! Unlinked downloads: downloaded files that no download pin needs any more.
//!
//! A pin records who asked for a song's audio (see `download_pins`): an
//! album or playlist, or the song itself. A file stays while any pin needs
//! it. A collection's pin stops counting when the user's library no longer
//! has the song in that collection (it left the playlist upstream) or the
//! collection itself is gone; such pins are *stale*. A file whose pins are
//! all stale, or that has none, is *unlinked*.
//!
//! Only collections the app registered when they were downloaded from the
//! library (`download_collections`) are checked: a pin owned by anything
//! else (a link pasted in the Downloads page, an individual download) is
//! never stale. Custom songs' audio, files a custom song plays by
//! reference, songs with an unfinished queue row, and files outside the
//! media folder are never unlinked. Links to local files live in
//! `track_mappings` and are never touched.
//!
//! [`unlinked_downloads`] only reports; the queue's
//! [`crate::DownloadQueue::clean_up_unlinked`] deletes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::storage::{DownloadCollection, LibraryFilter, Storage, StorageError, UnlinkedInputs};

/// A downloaded file no pin needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkedFile {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// The path as stored in the database.
    pub(crate) stored: String,
    /// Download keys (catalog ids) of the songs recording it.
    pub keys: Vec<String>,
    /// Size in bytes (`0` when the file is already gone).
    pub size: u64,
}

/// What [`unlinked_downloads`] found, or what a clean-up removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnlinkedReport {
    /// Number of files.
    pub files: usize,
    /// Their total size in bytes.
    pub bytes: u64,
}

impl UnlinkedReport {
    fn of(files: &[UnlinkedFile]) -> Self {
        Self {
            files: files.len(),
            bytes: files.iter().map(|file| file.size).sum(),
        }
    }
}

/// The stale pins and unlinked files of the store right now.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Pins whose collection no longer holds the song.
    pub stale_pins: Vec<(String, String)>,
    /// Files that no valid pin needs.
    pub files: Vec<UnlinkedFile>,
}

/// Lists the unlinked downloads (see the module docs) without changing
/// anything; `media_dir` is the folder downloads are written to.
///
/// # Errors
///
/// Returns database errors.
pub async fn unlinked_downloads(
    storage: &Storage,
    media_dir: &Path,
) -> Result<(Vec<UnlinkedFile>, UnlinkedReport), StorageError> {
    let scan = scan(storage, media_dir).await?;
    let report = UnlinkedReport::of(&scan.files);
    Ok((scan.files, report))
}

/// Finds stale pins and unlinked files.
pub(crate) async fn scan(storage: &Storage, media_dir: &Path) -> Result<Scan, StorageError> {
    let collections = storage.download_collections().await?;
    let pins = storage.download_pins().await?;
    let inputs = storage.unlinked_inputs().await?;
    let mut members = HashMap::new();
    for collection in &collections {
        let ids = storage
            .collection_members(collection.filter, &collection.owner)
            .await?;
        members.insert(
            collection.owner.clone(),
            ids.map(|ids| member_keys(&ids, &inputs.song_of)),
        );
    }
    let stale = stale_pins(&pins, &collections, &members, inputs.library_synced);
    let stale_set: HashSet<&(String, String)> = stale.iter().collect();
    let valid: HashSet<&str> = pins
        .iter()
        .filter(|pin| !stale_set.contains(pin))
        .map(|(track_id, _)| track_id.as_str())
        .collect();
    let mut files = unlinked_files(&inputs, &valid, |stored| {
        let path = storage.resolve_stored_path(stored);
        path.starts_with(media_dir).then_some(path)
    });
    for file in &mut files {
        if file.size == 0 {
            file.size = tokio::fs::metadata(&file.path)
                .await
                .map_or(0, |meta| meta.len());
        }
    }
    Ok(Scan {
        stale_pins: stale,
        files,
    })
}

/// Download keys (song ids) a collection's member ids stand for: each id
/// resolved through `song_of` (alias -> song id).
fn member_keys(ids: &[String], song_of: &HashMap<String, String>) -> HashSet<String> {
    ids.iter()
        .map(|id| song_of.get(id).cloned().unwrap_or_else(|| id.clone()))
        .collect()
}

/// Pins of registered collections that no longer hold the song. `members`
/// maps each registered owner to its member keys, or `None` when the
/// collection is not in the library view any more; a missing collection only
/// makes its pins stale for the Apple Music view of a library that was
/// synced (an unsynced library proves nothing).
fn stale_pins(
    pins: &[(String, String)],
    collections: &[DownloadCollection],
    members: &HashMap<String, Option<HashSet<String>>>,
    library_synced: bool,
) -> Vec<(String, String)> {
    let filters: HashMap<&str, LibraryFilter> = collections
        .iter()
        .map(|c| (c.owner.as_str(), c.filter))
        .collect();
    pins.iter()
        .filter(|(track_id, owner)| {
            let Some(filter) = filters.get(owner.as_str()) else {
                return false;
            };
            match members.get(owner) {
                Some(Some(keys)) => !keys.contains(track_id),
                Some(None) => *filter == LibraryFilter::AppleMusic && library_synced,
                None => false,
            }
        })
        .cloned()
        .collect()
}

/// Groups the audio rows by file and keeps the files none of whose songs is
/// pinned (`pinned` holds the keys of the valid pins) or protected.
/// `resolve` turns a stored path into the absolute path of a file inside the
/// media folder, or `None` for anything else.
fn unlinked_files(
    inputs: &UnlinkedInputs,
    pinned: &HashSet<&str>,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<UnlinkedFile> {
    // Rows arrive ordered by path.
    let mut by_path: Vec<(&str, Vec<&crate::storage::StoredFile>)> = Vec::new();
    for file in &inputs.files {
        match by_path.last_mut() {
            Some((path, rows)) if *path == file.path => rows.push(file),
            _ => by_path.push((file.path.as_str(), vec![file])),
        }
    }
    let mut out = Vec::new();
    for (stored, rows) in by_path {
        if inputs.custom_paths.contains(stored) {
            continue;
        }
        if rows.iter().any(|row| {
            pinned.contains(row.track_id.as_str()) || inputs.protected.contains(&row.track_id)
        }) {
            continue;
        }
        let Some(path) = resolve(stored) else {
            continue;
        };
        let mut keys: Vec<String> = rows.iter().map(|row| row.track_id.clone()).collect();
        keys.sort();
        keys.dedup();
        out.push(UnlinkedFile {
            path,
            stored: stored.to_owned(),
            keys,
            size: rows.iter().find_map(|row| row.size).unwrap_or(0),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{DownloadSourceKind, StoredFile};

    fn file(path: &str, track_id: &str) -> StoredFile {
        StoredFile {
            path: path.to_owned(),
            track_id: track_id.to_owned(),
            size: Some(10),
        }
    }

    fn resolve(stored: &str) -> Option<PathBuf> {
        stored
            .starts_with("media/")
            .then(|| PathBuf::from("/data").join(stored))
    }

    fn pinned<'a>(keys: &[&'a str]) -> HashSet<&'a str> {
        keys.iter().copied().collect()
    }

    fn inputs(files: Vec<StoredFile>) -> UnlinkedInputs {
        UnlinkedInputs {
            files,
            ..UnlinkedInputs::default()
        }
    }

    #[test]
    fn a_file_without_any_pin_should_be_unlinked() {
        let found = unlinked_files(
            &inputs(vec![file("media/1.m4a", "1")]),
            &pinned(&[]),
            resolve,
        );
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_file_two_songs_record_should_stay_while_one_is_pinned() {
        let found = unlinked_files(
            &inputs(vec![file("media/1.m4a", "1"), file("media/1.m4a", "2")]),
            &pinned(&["2"]),
            resolve,
        );
        assert_eq!(found.len(), 0);
    }

    #[test]
    fn a_file_a_custom_song_uses_should_stay() {
        let mut data = inputs(vec![file("media/1.m4a", "1")]);
        data.custom_paths.insert("media/1.m4a".to_owned());
        assert_eq!(unlinked_files(&data, &pinned(&[]), resolve).len(), 0);
    }

    #[test]
    fn a_song_a_custom_song_plays_by_reference_should_stay() {
        let mut data = inputs(vec![file("media/1.m4a", "1")]);
        data.protected.insert("1".to_owned());
        assert_eq!(unlinked_files(&data, &pinned(&[]), resolve).len(), 0);
    }

    #[test]
    fn a_file_outside_the_media_folder_should_never_be_unlinked() {
        let found = unlinked_files(
            &inputs(vec![file("C:/Music/a.m4a", "1")]),
            &pinned(&[]),
            resolve,
        );
        assert_eq!(found.len(), 0);
    }

    #[test]
    fn both_mixes_of_an_unpinned_song_should_be_unlinked() {
        let found = unlinked_files(
            &inputs(vec![
                file("media/1.m4a", "1"),
                file("media/1.spatial.m4a", "1"),
            ]),
            &pinned(&[]),
            resolve,
        );
        assert_eq!(found.len(), 2);
    }

    fn collection(owner: &str) -> DownloadCollection {
        DownloadCollection {
            owner: owner.to_owned(),
            filter: LibraryFilter::AppleMusic,
            kind: DownloadSourceKind::Playlist,
            name: String::new(),
            keep: false,
        }
    }

    fn pin(track: &str, owner: &str) -> (String, String) {
        (track.to_owned(), owner.to_owned())
    }

    #[test]
    fn a_song_that_left_its_playlist_should_have_a_stale_pin() {
        let members = HashMap::from([("p.1".to_owned(), Some(HashSet::from(["2".to_owned()])))]);
        let stale = stale_pins(
            &[pin("1", "p.1"), pin("2", "p.1")],
            &[collection("p.1")],
            &members,
            true,
        );
        assert_eq!(stale, [pin("1", "p.1")]);
    }

    #[test]
    fn a_deleted_playlist_should_make_all_its_pins_stale() {
        let members = HashMap::from([("p.1".to_owned(), None)]);
        let stale = stale_pins(&[pin("1", "p.1")], &[collection("p.1")], &members, true);
        assert_eq!(stale, [pin("1", "p.1")]);
    }

    #[test]
    fn an_unsynced_library_should_never_make_pins_stale() {
        let members = HashMap::from([("p.1".to_owned(), None)]);
        let stale = stale_pins(&[pin("1", "p.1")], &[collection("p.1")], &members, false);
        assert_eq!(stale.len(), 0);
    }

    #[test]
    fn pins_of_unregistered_owners_and_single_songs_should_never_be_stale() {
        let stale = stale_pins(
            &[pin("1", "pl.web"), pin("2", "")],
            &[],
            &HashMap::new(),
            true,
        );
        assert_eq!(stale.len(), 0);
    }

    /// Counts the unlinked downloads of a COPY of a real database (never the
    /// live file; opening it migrates it). Run with `CIDER_DB_COPY` set:
    /// `cargo test -p cider-offline-core real_db_copy_unlinked -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs CIDER_DB_COPY pointing at a copy of a real store.sqlite"]
    #[expect(
        clippy::print_stdout,
        reason = "a manual check that reports what it saw"
    )]
    async fn real_db_copy_unlinked() {
        let path = PathBuf::from(std::env::var_os("CIDER_DB_COPY").expect("CIDER_DB_COPY"));
        let storage = Storage::open_async(path.clone()).await.unwrap();
        let media = path.parent().unwrap().join("media");
        let scan = scan(&storage, &media).await.unwrap();
        println!("stale pins: {}", scan.stale_pins.len());
        println!("unlinked files: {}", scan.files.len());
        for file in &scan.files {
            println!("  {} keys={:?}", file.stored, file.keys);
        }
    }

    #[test]
    fn member_keys_should_resolve_aliases_to_song_ids() {
        let songs = HashMap::from([("i.1".to_owned(), "1".to_owned())]);
        let keys = member_keys(&["i.1".to_owned(), "2".to_owned()], &songs);
        assert_eq!(keys, HashSet::from(["1".to_owned(), "2".to_owned()]));
    }
}
