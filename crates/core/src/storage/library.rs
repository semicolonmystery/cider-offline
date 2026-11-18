//! The "library" view: the collections and tracks of the user's *actual* Apple
//! Music library, or — in the other modes — the downloaded, the whole cache, or
//! the user's custom content.
//!
//! # Apple Music library = the API-synced snapshot, nothing else
//!
//! [`LibraryFilter::AppleMusic`] reads ONLY the `library_collections` /
//! `library_collection_tracks` tables that the active library sync
//! ([`crate::library_sync`]) fills straight from the Apple Music API, plus the
//! `tracks` rows those reference. It never synthesizes albums from songs and
//! never consults passively captured Cider traffic, so it shows exactly the
//! library playlists and albums Apple returns, each with its complete track
//! list in Apple's order. Ordering: playlists first (the system "Favourite
//! Songs" playlist pinned first), then albums; within each, names compare
//! case-insensitively and accent-insensitively (`Český` sorts with `C`).
//!
//! # Dedup (other views)
//!
//! The same song can be cached under both a catalog id (numeric, type `songs`)
//! and a library id (`i.…`, type `library-songs`), the library row carrying the
//! catalog id of its twin in `catalog_id`. Within any card, members are deduped
//! by `catalog_id` (falling back to the row id, which *is* the catalog id for the
//! catalog twin), so the pair collapses to one. [`LibraryFilter::Downloaded`]
//! additionally drops a plain catalog row at the SQL level when a library row
//! references it.
//!
//! # Album grouping (other views)
//!
//! In the cache-wide views songs are grouped under their album: a song joins a
//! stored album when it references it (`relationships.albums.data[0].id`, or a
//! matching name + artist); otherwise its album is synthesized from the song's
//! own `albumName` / `albumArtistName`. The synced library collections are
//! included there too, with their API membership.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde_json::Value;

use super::models::{CollectionKind, Playlist, Track};
use super::paths::PathCodec;
use super::repo::{PLAYLIST_COLUMNS, TRACK_COLUMNS, playlist_from_row, track_from_row};
use super::{Storage, StorageError};

/// Which set the library view returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LibraryFilter {
    /// Exactly the user's real Apple Music library as last synced from the
    /// Apple Music API: every library playlist and library album, each with its
    /// complete, Apple-ordered track list. No synthesized albums.
    #[default]
    AppleMusic,
    /// Collections that have at least one track downloaded locally (a local
    /// audio file or a completed download), regardless of library membership.
    Downloaded,
    /// The entire cache: every album, playlist and song observed, with no
    /// membership restriction.
    All,
    /// Everything user-created or user-modified: custom songs / collections and
    /// any track whose audio the user supplied or changed, plus any album or
    /// playlist that contains such a track.
    Custom,
}

/// A collection in the library view together with its member track ids, in
/// order. For an album these are only the user's *saved* (or, in
/// [`LibraryFilter::Downloaded`], downloaded) songs of that album, not the whole
/// catalog track list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCollection {
    /// The album / playlist (a stored row, or a synthesized album group).
    pub collection: Playlist,
    /// Member track ids, ordered.
    pub track_ids: Vec<String>,
}

/// The full result of a library query: the collections grid and the flat track
/// list, both already filtered and deduped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryContents {
    /// Albums and playlists to show, sorted by name.
    pub collections: Vec<LibraryCollection>,
    /// Flat track list (every library / downloaded track), deduped.
    pub tracks: Vec<Track>,
}

// ----- Album identity keys ----------------------------------------------------

/// A case-folded, trimmed form for name comparison.
fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Composite "name + artist" album key, the fallback identity when no id links a
/// song to a stored album.
fn name_key(name: &str, artist: &str) -> String {
    format!("{}\u{1}{}", norm(name), norm(artist))
}

fn parse_metadata(metadata: Option<&str>) -> Option<Value> {
    metadata.and_then(|m| serde_json::from_str(m).ok())
}

/// The album-**identity** keys a song shares with every other song of the same
/// album and with its stored library-album row. Namespaced so an id can never
/// collide with a name. Crucially this NEVER keys on the song's own
/// `artistName` (which varies per track with featured artists) — only on the
/// referenced album id and the album's own name / album-artist. Empty when the
/// song carries no album identity (a true standalone single).
fn album_identity_keys_for_track(track: &Track) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(value) = parse_metadata(track.metadata.as_deref())
        && let Some(id) = value
            .pointer("/relationships/albums/data/0/id")
            .and_then(Value::as_str)
    {
        keys.push(format!("id:{id}"));
    }
    if let Some(album) = track.album.as_deref().filter(|a| !a.is_empty()) {
        let name = norm(album);
        // Name-only key: the reliable link across tracks whose album-artist is
        // missing or inconsistent, and to the stored album of the same name.
        keys.push(format!("n:{name}"));
        if let Some(album_artist) = track.album_artist.as_deref().filter(|a| !a.is_empty()) {
            keys.push(format!("na:{}", name_key(album, album_artist)));
        }
    }
    keys
}

/// The album-identity keys a stored album row can be matched by: its id, any
/// catalog twin id in its metadata, and its name / name+artist keys.
fn album_identity_keys_for_collection(playlist: &Playlist) -> Vec<String> {
    let mut keys = vec![format!("id:{}", playlist.id)];
    if let Some(value) = parse_metadata(playlist.metadata.as_deref()) {
        for pointer in [
            "/relationships/catalog/data/0/id",
            "/attributes/playParams/catalogId",
            "/attributes/playParams/id",
            "/id",
        ] {
            if let Some(id) = value.pointer(pointer).and_then(Value::as_str) {
                keys.push(format!("id:{id}"));
            }
        }
    }
    let name = norm(&playlist.name);
    if !name.is_empty() {
        keys.push(format!("n:{name}"));
        if let Some(artist) = playlist.artist.as_deref().filter(|a| !a.is_empty()) {
            keys.push(format!("na:{}", name_key(&playlist.name, artist)));
        }
    }
    keys
}

/// Minimal union-find over node indices, used to consolidate every song and the
/// stored album row that share an album-identity key into one group.
struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new() -> Self {
        Self { parent: Vec::new() }
    }

    fn make(&mut self) -> usize {
        let index = self.parent.len();
        self.parent.push(index);
        index
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cursor = x;
        while self.parent[cursor] != cursor {
            let next = self.parent[cursor];
            self.parent[cursor] = root;
            cursor = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// Registers `node` under each of `keys`, unioning it with the node that already
/// owns a shared key (that is how a song joins its album's group).
fn register_keys(
    set: &mut DisjointSet,
    key_owner: &mut HashMap<String, usize>,
    node: usize,
    keys: Vec<String>,
) {
    for key in keys {
        match key_owner.get(&key) {
            Some(&owner) => set.union(node, owner),
            None => {
                key_owner.insert(key, node);
            }
        }
    }
}

/// Insertion index a track wants within its album (0-based `track_number`),
/// falling back to the end so numberless tracks keep a stable relative order.
fn track_order(track: &Track) -> (u32, u32, String) {
    (
        track.disc_number.unwrap_or(1),
        track.track_number.unwrap_or(u32::MAX),
        track.title.to_lowercase(),
    )
}

// ----- Assembly (pure) --------------------------------------------------------

/// The within-card dedup key for a track: its catalog id when known, else its
/// own id — which, for a catalog-native row, *is* the catalog id, so a library
/// song and its catalog twin collapse to one. (A stronger key than a bare title,
/// which would wrongly merge two distinct same-titled songs.)
fn member_key(track: &Track) -> String {
    track.catalog_id.clone().unwrap_or_else(|| track.id.clone())
}

/// Drops later duplicates of the same song within one card, by [`member_key`],
/// keeping the first (already in the card's display order).
fn dedupe_members(members: Vec<&Track>) -> Vec<&Track> {
    let mut seen = std::collections::HashSet::new();
    members
        .into_iter()
        .filter(|track| seen.insert(member_key(track)))
        .collect()
}

/// Builds the collections grid from the eligible `tracks`, the stored
/// album-kind `albums` and playlist-kind `playlists`, and each playlist's
/// ordered `membership`. `require_album_members` / `require_playlist_members`
/// drop a card of that kind with no eligible member. `tracks` is the flat list
/// verbatim.
fn assemble(
    tracks: Vec<Track>,
    albums: Vec<Playlist>,
    playlists: Vec<Playlist>,
    membership: &HashMap<String, Vec<String>>,
    require_album_members: bool,
    require_playlist_members: bool,
) -> LibraryContents {
    let mut collections = album_collections(&tracks, albums, require_album_members);
    collections.extend(playlist_collections(
        &tracks,
        playlists,
        membership,
        require_playlist_members,
    ));
    collections.sort_by(|a, b| {
        norm(&a.collection.name)
            .cmp(&norm(&b.collection.name))
            .then_with(|| a.collection.id.cmp(&b.collection.id))
    });
    LibraryContents {
        collections,
        tracks,
    }
}

/// Consolidates stored album rows and every saved song into album groups: one
/// node per stored album and per song, unioned whenever they share an
/// album-identity key. So all of an album's saved songs — however they arrived,
/// and whatever their per-track featured artists — plus the stored library-album
/// row end up in one canonical card (and duplicate stored album rows collapse).
fn album_collections(
    tracks: &[Track],
    albums: Vec<Playlist>,
    require_members: bool,
) -> Vec<LibraryCollection> {
    let mut set = DisjointSet::new();
    let mut key_owner: HashMap<String, usize> = HashMap::new();
    let mut node_album: Vec<Option<Playlist>> = Vec::new();
    let mut node_tracks: Vec<Vec<&Track>> = Vec::new();

    for album in albums {
        let keys = album_identity_keys_for_collection(&album);
        let node = set.make();
        node_album.push(Some(album));
        node_tracks.push(Vec::new());
        register_keys(&mut set, &mut key_owner, node, keys);
    }
    for track in tracks {
        let keys = album_identity_keys_for_track(track);
        if keys.is_empty() {
            continue; // a standalone single with no album identity — flat list only
        }
        let node = set.make();
        node_album.push(None);
        node_tracks.push(vec![track]);
        register_keys(&mut set, &mut key_owner, node, keys);
    }

    // Fold every node into its group root: the first stored album row wins
    // (duplicates dropped), with the union of member songs.
    let mut groups: HashMap<usize, (Option<Playlist>, Vec<&Track>)> = HashMap::new();
    for node in 0..node_tracks.len() {
        let root = set.find(node);
        let group = groups.entry(root).or_insert_with(|| (None, Vec::new()));
        if group.0.is_none()
            && let Some(album) = node_album[node].take()
        {
            group.0 = Some(album);
        }
        group.1.append(&mut node_tracks[node]);
    }

    let mut collections = Vec::new();
    for (_, (album, mut members)) in groups {
        members.sort_by_key(|track| track_order(track));
        let members = dedupe_members(members);
        // No stored row: synthesize the album from a representative member (one
        // carrying album-artist when possible), else skip an empty group.
        let collection = album.or_else(|| {
            members
                .iter()
                .find(|t| t.album_artist.is_some())
                .or_else(|| members.first())
                .map(|track| synthesized_album(track))
        });
        let Some(collection) = collection else {
            continue;
        };
        if require_members && members.is_empty() {
            continue;
        }
        collections.push(LibraryCollection {
            collection,
            track_ids: members.into_iter().map(|t| t.id.clone()).collect(),
        });
    }
    collections
}

/// Playlists with their stored membership filtered to the eligible track set,
/// collapsing duplicate rows (same normalized name) into one with merged members
/// so a playlist like "Acoustic Morning" appears exactly once.
fn playlist_collections(
    tracks: &[Track],
    playlists: Vec<Playlist>,
    membership: &HashMap<String, Vec<String>>,
    require_members: bool,
) -> Vec<LibraryCollection> {
    let by_id: HashMap<&str, &Track> = tracks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut rows: Vec<LibraryCollection> = Vec::new();
    // Per-row set of already-placed member keys, so a song (and its catalog
    // twin) is never listed twice, including across merged duplicate playlists.
    let mut seen_keys: Vec<std::collections::HashSet<String>> = Vec::new();
    for playlist in playlists {
        // Eligible, in-order member ids of this playlist row.
        let members: Vec<(String, String)> = membership
            .get(&playlist.id)
            .into_iter()
            .flatten()
            .filter_map(|id| {
                by_id
                    .get(id.as_str())
                    .map(|track| (id.clone(), member_key(track)))
            })
            .collect();
        let name_key = norm(&playlist.name);
        let row_index = if let Some(&existing) = index.get(&name_key) {
            existing
        } else {
            index.insert(name_key, rows.len());
            rows.push(LibraryCollection {
                collection: playlist,
                track_ids: Vec::new(),
            });
            seen_keys.push(std::collections::HashSet::new());
            rows.len() - 1
        };
        for (id, key) in members {
            if seen_keys[row_index].insert(key) {
                rows[row_index].track_ids.push(id);
            }
        }
    }
    rows.retain(|row| !require_members || !row.track_ids.is_empty());
    rows
}

/// A synthesized album row from one of its member songs (not a stored resource;
/// `is_custom = false`, no reserved id — it borrows the album's identity). The
/// subtitle artist is the **album** artist (`albumArtistName`), never the song's
/// own per-track `artistName`, so featured artists do not appear on the card.
fn synthesized_album(track: &Track) -> Playlist {
    let name = track.album.clone().unwrap_or_default();
    let artist = track.album_artist.clone().filter(|a| !a.is_empty());
    Playlist {
        id: format!("album-group:{}", norm(&name)),
        kind: CollectionKind::Album,
        name,
        artist,
        artwork_url: track.artwork_url.clone(),
        metadata: None,
        is_custom: false,
        description: None,
        genre: track.genre.clone(),
        release_date: track.release_date.clone(),
    }
}

// ----- Queries ----------------------------------------------------------------

/// Display order of every flat track list (`id` breaks ties so paged
/// windows never overlap or skip rows).
pub(super) const TRACK_ORDER: &str = "t.artist, t.album, t.title, t.id";

/// SQL predicate over `tracks t` selecting the flat track list of `filter`.
/// Shared by [`Storage::library_contents`] and the paged queries, so both
/// always agree.
pub(super) fn track_predicate(filter: LibraryFilter) -> &'static str {
    match filter {
        // Only the API-synced snapshot: the tracks Apple returned for the
        // synced collections. Passive capture never reaches here.
        LibraryFilter::AppleMusic => "t.id IN (SELECT track_id FROM library_collection_tracks)",
        // A track is downloaded if it has a local audio file or a completed
        // download row. A plain catalog row is dropped when a library row
        // references it as its catalog twin (dedup of the same song under
        // both ids).
        LibraryFilter::Downloaded => {
            "(t.audio_path IS NOT NULL
              OR EXISTS (
                  SELECT 1 FROM downloads d
                  WHERE (d.track_id = t.id OR d.track_id = t.catalog_id) AND d.status = 'done'
              ))
             AND NOT EXISTS (
                 SELECT 1 FROM tracks l WHERE l.catalog_id = t.id AND l.id <> t.id
             )"
        }
        LibraryFilter::All => "1",
        LibraryFilter::Custom => CUSTOM_TRACK_PREDICATE,
    }
}

/// The flat track list of `filter`, in display order.
fn filter_tracks(
    conn: &Connection,
    codec: &PathCodec,
    filter: LibraryFilter,
) -> Result<Vec<Track>, StorageError> {
    let columns = TRACK_COLUMNS
        .split(", ")
        .map(|column| format!("t.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    load_tracks(
        conn,
        codec,
        &format!(
            "SELECT {columns} FROM tracks t WHERE {} ORDER BY {TRACK_ORDER}",
            track_predicate(filter)
        ),
    )
}

/// The full library view of `filter` (blocking; runs on the connection).
pub(super) fn contents(
    conn: &Connection,
    codec: &PathCodec,
    filter: LibraryFilter,
) -> Result<LibraryContents, StorageError> {
    match filter {
        LibraryFilter::AppleMusic => apple_music_contents(conn, codec),
        LibraryFilter::Downloaded => downloaded_contents(conn, codec),
        LibraryFilter::All => all_contents(conn, codec),
        LibraryFilter::Custom => custom_contents(conn, codec),
    }
}

fn load_tracks(
    conn: &Connection,
    codec: &PathCodec,
    sql: &str,
) -> Result<Vec<Track>, StorageError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], track_from_row(codec))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_collections(conn: &Connection, sql: &str) -> Result<Vec<Playlist>, StorageError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], playlist_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn membership_for(
    conn: &Connection,
    collections: &[Playlist],
) -> Result<HashMap<String, Vec<String>>, StorageError> {
    let mut out = HashMap::new();
    for collection in collections {
        let ids = super::repo::playlist_track_ids(conn, &collection.id)?;
        if !ids.is_empty() {
            out.insert(collection.id.clone(), ids);
        }
    }
    Ok(out)
}

/// Splits loaded collections into album-kind and playlist-kind.
fn split_kinds(collections: Vec<Playlist>) -> (Vec<Playlist>, Vec<Playlist>) {
    collections
        .into_iter()
        .partition(|c| c.kind == CollectionKind::Album)
}

/// Folds a name for display ordering: trimmed, lower-cased and with Latin
/// diacritics removed, so `Crème Brûlée` sorts between `CAFE` and `Dinner`.
pub(crate) fn sort_fold(name: &str) -> String {
    name.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .map(strip_diacritic)
        .collect()
}

/// Base letter of a lower-case Latin-1 / Latin Extended-A letter.
fn strip_diacritic(c: char) -> char {
    match c {
        'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    }
}

/// Whether a library playlist is the system "Favourite Songs" playlist, which
/// Apple / Cider pin above the user's own playlists. Apple marks it as neither
/// editable nor deletable; the name check covers both spellings.
fn is_pinned(collection: &Playlist) -> bool {
    if collection.kind != CollectionKind::Playlist {
        return false;
    }
    let name = sort_fold(&collection.name);
    if name == "favourite songs" || name == "favorite songs" {
        return true;
    }
    parse_metadata(collection.metadata.as_deref()).is_some_and(|value| {
        value
            .pointer("/attributes/canEdit")
            .and_then(Value::as_bool)
            == Some(false)
            && value
                .pointer("/attributes/canDelete")
                .and_then(Value::as_bool)
                == Some(false)
    })
}

/// Display order of the Apple Music library grid: playlists (pinned first)
/// before albums, then by folded name, then by raw name and id for stability.
fn apple_music_order(a: &Playlist, b: &Playlist) -> std::cmp::Ordering {
    let rank = |c: &Playlist| {
        (
            u8::from(c.kind == CollectionKind::Album),
            u8::from(!is_pinned(c)),
        )
    };
    rank(a)
        .cmp(&rank(b))
        .then_with(|| sort_fold(&a.name).cmp(&sort_fold(&b.name)))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.id.cmp(&b.id))
}

fn apple_music_contents(
    conn: &Connection,
    codec: &PathCodec,
) -> Result<LibraryContents, StorageError> {
    // Only the API-synced snapshot: the collections Apple listed and the
    // tracks Apple returned for them. Passive capture never reaches here.
    let synced = super::synced::load_synced(conn)?;
    let tracks = filter_tracks(conn, codec, LibraryFilter::AppleMusic)?;
    let mut collections: Vec<LibraryCollection> = synced
        .into_iter()
        .map(|synced| {
            let mut collection = synced.collection;
            collection.name = collection.name.trim().to_owned();
            LibraryCollection {
                collection,
                track_ids: synced.track_ids,
            }
        })
        .collect();
    collections.sort_by(|a, b| apple_music_order(&a.collection, &b.collection));
    Ok(LibraryContents {
        collections,
        tracks,
    })
}

/// Collections with their ordered member ids, keyed by collection id.
type CollectionsWithMembers = (Vec<Playlist>, HashMap<String, Vec<String>>);

/// Adds the synced library collections (with their authoritative API
/// membership) to a cache-wide candidate set, replacing any passively captured
/// row with the same id.
fn with_synced(
    conn: &Connection,
    mut collections: Vec<Playlist>,
    mut membership: HashMap<String, Vec<String>>,
) -> Result<CollectionsWithMembers, StorageError> {
    let synced = super::synced::load_synced(conn)?;
    let synced_ids: HashSet<&str> = synced.iter().map(|s| s.collection.id.as_str()).collect();
    collections.retain(|c| !synced_ids.contains(c.id.as_str()));
    drop(synced_ids);
    for entry in synced {
        membership.insert(entry.collection.id.clone(), entry.track_ids);
        collections.push(entry.collection);
    }
    Ok((collections, membership))
}

fn downloaded_contents(
    conn: &Connection,
    codec: &PathCodec,
) -> Result<LibraryContents, StorageError> {
    // See `track_predicate` for what "downloaded" means.
    let tracks = filter_tracks(conn, codec, LibraryFilter::Downloaded)?;
    // Every stored collection is a candidate; `require_members` keeps only those
    // with a downloaded member.
    let collections = load_collections(
        conn,
        &format!("SELECT {PLAYLIST_COLUMNS} FROM playlists ORDER BY name"),
    )?;
    let membership = membership_for(conn, &collections)?;
    let (collections, membership) = with_synced(conn, collections, membership)?;
    let (albums, playlists) = split_kinds(collections);
    Ok(assemble(tracks, albums, playlists, &membership, true, true))
}

fn all_contents(conn: &Connection, codec: &PathCodec) -> Result<LibraryContents, StorageError> {
    // The entire cache, no membership restriction. Every stored collection is
    // shown (empties kept); synthesized album groups always have a member.
    let tracks = filter_tracks(conn, codec, LibraryFilter::All)?;
    let collections = load_collections(
        conn,
        &format!("SELECT {PLAYLIST_COLUMNS} FROM playlists ORDER BY name"),
    )?;
    let membership = membership_for(conn, &collections)?;
    let (collections, membership) = with_synced(conn, collections, membership)?;
    let (albums, playlists) = split_kinds(collections);
    Ok(assemble(
        tracks,
        albums,
        playlists,
        &membership,
        false,
        false,
    ))
}

/// SQL predicate (over a `tracks` row aliased `t`) selecting a user-created or
/// user-modified track: a custom song, a reference-audio song, or any row
/// whose audio the user supplied/changed (`audio_mode` set; `NULL` is the
/// untouched default for real tracks).
const CUSTOM_TRACK_PREDICATE: &str =
    "(t.is_custom = 1 OR t.reference_track_id IS NOT NULL OR t.audio_mode IS NOT NULL)";

fn custom_contents(conn: &Connection, codec: &PathCodec) -> Result<LibraryContents, StorageError> {
    let tracks = filter_tracks(conn, codec, LibraryFilter::Custom)?;
    // Custom collections, plus any collection that contains a custom track.
    let collections = load_collections(
        conn,
        &format!(
            "SELECT {PLAYLIST_COLUMNS} FROM playlists p
             WHERE p.is_custom = 1
                OR p.id IN (
                    SELECT pt.playlist_id FROM playlist_tracks pt
                    JOIN tracks t ON t.id = pt.track_id
                    WHERE {CUSTOM_TRACK_PREDICATE}
                )
             ORDER BY p.name"
        ),
    )?;
    let membership = membership_for(conn, &collections)?;
    let (albums, playlists) = split_kinds(collections);
    // Custom collections are shown even when empty; album groups synthesized
    // from custom tracks always carry their member.
    Ok(assemble(
        tracks,
        albums,
        playlists,
        &membership,
        false,
        false,
    ))
}

impl Storage {
    /// Returns the library view for `filter`: the collections grid and flat
    /// track list that mirror the user's Apple Music library, or the collections
    /// with downloaded audio. See [the module docs](self) for the signal, dedup
    /// and grouping rules.
    ///
    /// # Errors
    ///
    /// Returns database errors.
    pub async fn library_contents(
        &self,
        filter: LibraryFilter,
    ) -> Result<LibraryContents, StorageError> {
        let codec = self.codec.clone();
        self.call(move |c| contents(c, &codec, filter)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CatalogUpdate, CollectionKind, NewCustomTrack, TrackMetadata};
    use std::path::PathBuf;

    /// A catalog (browse) song resource JSON.
    fn catalog_song_meta(id: &str, album: &str) -> String {
        serde_json::json!({
            "id": id, "type": "songs",
            "attributes": {"name": format!("song {id}"), "artistName": "Art", "albumName": album}
        })
        .to_string()
    }

    /// A library song resource JSON referencing a (library) album id.
    fn library_song_meta(id: &str, album_id: &str, album: &str) -> String {
        serde_json::json!({
            "id": id, "type": "library-songs",
            "attributes": {"name": format!("song {id}"), "artistName": "Art", "albumName": album},
            "relationships": {"albums": {"data": [{"id": album_id, "type": "library-albums"}]}}
        })
        .to_string()
    }

    fn library_album_meta(id: &str, name: &str) -> String {
        serde_json::json!({
            "id": id, "type": "library-albums",
            "attributes": {"name": name, "artistName": "Art"}
        })
        .to_string()
    }

    /// A track NOT proven to be a saved library member (`in_library = false`):
    /// a catalog-browse song, or a song seen only inside a playlist.
    fn track_meta(id: &str, album: &str, metadata: String) -> TrackMetadata {
        TrackMetadata {
            id: id.to_owned(),
            title: format!("song {id}"),
            artist: "Art".to_owned(),
            album: Some(album.to_owned()),
            metadata: Some(metadata),
            ..TrackMetadata::default()
        }
    }

    /// A song proven to be a saved library member (`in_library = true`), as the
    /// ingest layer tags songs from `/v1/me/library/songs`.
    fn library_track_meta(id: &str, album: &str, metadata: String) -> TrackMetadata {
        TrackMetadata {
            in_library: true,
            ..track_meta(id, album, metadata)
        }
    }

    /// A saved library song with a per-track (featured) artist, referencing an
    /// album id (`in_library = true`).
    fn featured_song(
        id: &str,
        artist: &str,
        album_id: &str,
        album: &str,
        track: u32,
    ) -> TrackMetadata {
        TrackMetadata {
            id: id.to_owned(),
            title: format!("song {id}"),
            artist: artist.to_owned(),
            album: Some(album.to_owned()),
            track_number: Some(track),
            metadata: Some(library_song_meta(id, album_id, album)),
            in_library: true,
            ..TrackMetadata::default()
        }
    }

    fn store() -> Storage {
        Storage::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn all_view_should_collapse_featured_artist_songs_to_one_album() {
        // The user's saved songs of one album, each with a DIFFERENT per-track
        // featured artist, arriving as library-songs alongside the stored
        // library-album row: all must sit under ONE non-empty album card.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![
                    featured_song(
                        "i.1",
                        "Example Artist & Guest One",
                        "l.examplealbum",
                        "Example Album (Deluxe)",
                        1,
                    ),
                    featured_song(
                        "i.2",
                        "Example Artist & Guest Two",
                        "l.examplealbum",
                        "Example Album (Deluxe)",
                        2,
                    ),
                    featured_song(
                        "i.3",
                        "Example Artist & Guest Three",
                        "l.examplealbum",
                        "Example Album (Deluxe)",
                        3,
                    ),
                ],
                collections: vec![Playlist {
                    metadata: Some(library_album_meta(
                        "l.examplealbum",
                        "Example Album (Deluxe)",
                    )),
                    ..Playlist::minimal(
                        "l.examplealbum",
                        CollectionKind::Album,
                        "Example Album (Deluxe)",
                    )
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        let albums: Vec<&LibraryCollection> = contents
            .collections
            .iter()
            .filter(|c| c.collection.kind == CollectionKind::Album)
            .collect();
        // Exactly one album card, holding all three saved songs, and not empty.
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].collection.id, "l.examplealbum");
        assert_eq!(albums[0].track_ids.len(), 3);
        // No album card is empty.
        assert!(!albums.iter().any(|c| c.track_ids.is_empty()));
        // Members are ordered by track number.
        assert_eq!(
            albums[0].track_ids,
            vec!["i.1".to_owned(), "i.2".to_owned(), "i.3".to_owned()]
        );
    }

    #[tokio::test]
    async fn all_view_should_collapse_duplicate_playlists() {
        // The same playlist name captured under several ids must appear once.
        let storage = store();
        for id in ["p.1", "p.2", "p.3"] {
            storage
                .upsert_playlist(Playlist {
                    metadata: Some(
                        serde_json::json!({"id":id,"type":"library-playlists","attributes":{"name":"Acoustic Morning"}})
                            .to_string(),
                    ),
                    ..Playlist::minimal(id, CollectionKind::Playlist, "Acoustic Morning")
                })
                .await
                .unwrap();
        }
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        let playlists: Vec<&LibraryCollection> = contents
            .collections
            .iter()
            .filter(|c| c.collection.kind == CollectionKind::Playlist)
            .collect();
        assert_eq!(playlists.len(), 1);
    }

    #[tokio::test]
    async fn apple_music_should_exclude_catalog_browse_items() {
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![track_meta("123", "Browsed", catalog_song_meta("123", "Browsed"))],
                collections: vec![Playlist {
                    metadata: Some(
                        serde_json::json!({"id":"456","type":"albums","attributes":{"name":"Browsed"}})
                            .to_string(),
                    ),
                    ..Playlist::minimal("456", CollectionKind::Album, "Browsed")
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(contents.tracks, Vec::<Track>::new());
        assert_eq!(contents.collections, Vec::<LibraryCollection>::new());
    }

    #[tokio::test]
    async fn all_view_should_attach_song_to_its_library_album() {
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![library_track_meta(
                    "i.1",
                    "Deluxe",
                    library_song_meta("i.1", "l.al", "Deluxe"),
                )],
                collections: vec![Playlist {
                    metadata: Some(library_album_meta("l.al", "Deluxe")),
                    ..Playlist::minimal("l.al", CollectionKind::Album, "Deluxe")
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        assert_eq!(contents.collections.len(), 1);
        assert_eq!(contents.collections[0].collection.id, "l.al");
        assert_eq!(contents.collections[0].track_ids, vec!["i.1".to_owned()]);
        assert_eq!(contents.tracks.len(), 1);
    }

    #[tokio::test]
    async fn all_view_should_synthesize_album_of_single_song() {
        // A library song whose album was never added as a library-album still
        // shows as an album card with just that one song.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![library_track_meta(
                    "i.9",
                    "Orphan Album",
                    library_song_meta("i.9", "l.missing", "Orphan Album"),
                )],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        assert_eq!(contents.collections.len(), 1);
        let group = &contents.collections[0];
        assert!(!group.collection.is_custom);
        assert_eq!(group.collection.name, "Orphan Album");
        assert_eq!(group.track_ids, vec!["i.9".to_owned()]);
    }

    #[tokio::test]
    async fn all_view_should_not_duplicate_same_album_under_two_ids() {
        // Same album name cached as both a catalog album and a library album.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![library_track_meta(
                    "i.2",
                    "Example Album (Deluxe)",
                    library_song_meta("i.2", "l.examplealbum", "Example Album (Deluxe)"),
                )],
                collections: vec![
                    Playlist {
                        metadata: Some(library_album_meta("l.examplealbum", "Example Album (Deluxe)")),
                        ..Playlist::minimal("l.examplealbum", CollectionKind::Album, "Example Album (Deluxe)")
                    },
                    Playlist {
                        metadata: Some(
                            serde_json::json!({"id":"999","type":"albums","attributes":{"name":"Example Album (Deluxe)"}})
                                .to_string(),
                        ),
                        ..Playlist::minimal("999", CollectionKind::Album, "Example Album (Deluxe)")
                    },
                ],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        // Only the library album survives, and the song attaches to it (no synth).
        assert_eq!(contents.collections.len(), 1);
        assert_eq!(contents.collections[0].collection.id, "l.examplealbum");
    }

    #[tokio::test]
    async fn custom_items_should_appear_in_custom_view() {
        let storage = store();
        storage
            .create_custom_track(NewCustomTrack {
                title: "Mine".to_owned(),
                artist: "Me".to_owned(),
                album: Some("My Album".to_owned()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::Custom)
            .await
            .unwrap();
        assert_eq!(contents.tracks.len(), 1);
        assert_eq!(contents.collections.len(), 1);
        assert_eq!(contents.collections[0].collection.name, "My Album");
    }

    #[tokio::test]
    async fn custom_items_should_not_appear_in_apple_music_view() {
        // Custom content is NOT part of the real Apple Music library view.
        let storage = store();
        storage
            .create_custom_track(NewCustomTrack {
                title: "Mine".to_owned(),
                artist: "Me".to_owned(),
                album: Some("My Album".to_owned()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(contents.tracks, Vec::<Track>::new());
        assert_eq!(contents.collections, Vec::<LibraryCollection>::new());
    }

    #[tokio::test]
    async fn apple_music_should_exclude_playlist_only_songs() {
        // A song captured only as a playlist member (in_library = 0) must never
        // create or join an album card, nor appear in the flat song list.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![track_meta(
                    "i.p",
                    "Playlist Album",
                    library_song_meta("i.p", "l.pa", "Playlist Album"),
                )],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(contents.tracks, Vec::<Track>::new());
        assert_eq!(contents.collections, Vec::<LibraryCollection>::new());
    }

    #[tokio::test]
    async fn all_filter_should_include_catalog_browse_items() {
        // The All view shows the entire cache, including browse-cached catalog
        // songs and albums that the Apple Music view excludes.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![track_meta(
                    "123",
                    "Browsed",
                    catalog_song_meta("123", "Browsed"),
                )],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        assert_eq!(contents.tracks.len(), 1);
        // The browsed song synthesizes its album card.
        assert_eq!(contents.collections.len(), 1);
        assert_eq!(contents.collections[0].collection.name, "Browsed");
    }

    #[tokio::test]
    async fn all_filter_should_dedupe_catalog_twin_within_a_card() {
        // A library song and its catalog twin (same album) collapse to one
        // member in the All view's album card.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![
                    TrackMetadata {
                        catalog_id: Some("777".to_owned()),
                        ..library_track_meta("i.t", "Twin", library_song_meta("i.t", "l.t", "Twin"))
                    },
                    track_meta("777", "Twin", catalog_song_meta("777", "Twin")),
                ],
                collections: vec![Playlist {
                    metadata: Some(library_album_meta("l.t", "Twin")),
                    ..Playlist::minimal("l.t", CollectionKind::Album, "Twin")
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        let album = contents
            .collections
            .iter()
            .find(|c| c.collection.kind == CollectionKind::Album)
            .unwrap();
        // The library row and its catalog twin collapse to a single member.
        assert_eq!(album.track_ids.len(), 1);
    }

    #[tokio::test]
    async fn custom_filter_should_be_empty_without_custom_content() {
        // A pure saved-library cache has nothing custom.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![library_track_meta(
                    "i.1",
                    "Al",
                    library_song_meta("i.1", "l.al", "Al"),
                )],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::Custom)
            .await
            .unwrap();
        assert_eq!(contents.tracks, Vec::<Track>::new());
        assert_eq!(contents.collections, Vec::<LibraryCollection>::new());
    }

    #[tokio::test]
    async fn downloaded_should_only_include_downloaded_collections() {
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![
                    track_meta(
                        "i.a",
                        "Has Audio",
                        library_song_meta("i.a", "l.x", "Has Audio"),
                    ),
                    track_meta(
                        "i.b",
                        "No Audio",
                        library_song_meta("i.b", "l.y", "No Audio"),
                    ),
                ],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        storage
            .set_track_audio("i.a", Some(PathBuf::from("a.m4a")))
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::Downloaded)
            .await
            .unwrap();
        assert_eq!(contents.tracks.len(), 1);
        assert_eq!(contents.tracks[0].id, "i.a");
        assert_eq!(contents.collections.len(), 1);
        assert_eq!(contents.collections[0].track_ids, vec!["i.a".to_owned()]);
    }

    #[tokio::test]
    async fn downloaded_should_collapse_catalog_twin_of_a_library_song() {
        // A library song `i.c` (catalog twin `321`) and the catalog row `321`,
        // both with local audio: only the library row remains.
        let storage = store();
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![
                    TrackMetadata {
                        catalog_id: Some("321".to_owned()),
                        ..track_meta("i.c", "Twin", library_song_meta("i.c", "l.z", "Twin"))
                    },
                    track_meta("321", "Twin", catalog_song_meta("321", "Twin")),
                ],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        storage
            .set_track_audio("i.c", Some(PathBuf::from("c.m4a")))
            .await
            .unwrap();
        storage
            .set_track_audio("321", Some(PathBuf::from("c.m4a")))
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::Downloaded)
            .await
            .unwrap();
        assert_eq!(contents.tracks.len(), 1);
        assert_eq!(contents.tracks[0].id, "i.c");
    }

    // ----- API-synced Apple Music library ----------------------------------

    use crate::storage::{LibrarySnapshot, SyncedCollection, TrackListUpdate};

    fn synced(id: &str, kind: CollectionKind, name: &str, tracks: &[&str]) -> SyncedCollection {
        SyncedCollection {
            collection: Playlist::minimal(id, kind, name),
            track_ids: tracks.iter().map(|t| (*t).to_owned()).collect(),
            reported_total: u32::try_from(tracks.len()).ok(),
        }
    }

    fn song_meta(id: &str) -> TrackMetadata {
        track_meta(id, "Al", library_song_meta(id, "l.al", "Al"))
    }

    async fn synced_store(collections: Vec<SyncedCollection>) -> Storage {
        let storage = store();
        let tracks = collections
            .iter()
            .flat_map(|c| c.track_ids.iter())
            .map(|id| song_meta(id))
            .collect();
        storage
            .replace_library(LibrarySnapshot {
                collections,
                tracks,
                songs: Vec::new(),
            })
            .await
            .unwrap();
        storage
    }

    fn names(contents: &LibraryContents) -> Vec<&str> {
        contents
            .collections
            .iter()
            .map(|c| c.collection.name.as_str())
            .collect()
    }

    #[tokio::test]
    async fn apple_music_should_show_every_synced_track_of_a_playlist() {
        // No `in_library` tagging needed: the API track list is the membership.
        let storage = synced_store(vec![synced(
            "p.g",
            CollectionKind::Playlist,
            "Weekend Mix",
            &["i.3", "i.1", "i.2"],
        )])
        .await;
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(contents.collections[0].track_ids, ["i.3", "i.1", "i.2"]);
    }

    #[tokio::test]
    async fn apple_music_should_ignore_passively_captured_collections() {
        let storage = synced_store(vec![synced(
            "p.a",
            CollectionKind::Playlist,
            "Mine",
            &["i.1"],
        )])
        .await;
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![song_meta("i.9")],
                collections: vec![Playlist {
                    metadata: Some(library_album_meta("l.x", "18 Months")),
                    ..Playlist::minimal("l.x", CollectionKind::Album, "18 Months")
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(names(&contents), ["Mine"]);
    }

    #[tokio::test]
    async fn passive_partial_page_should_not_alter_synced_membership() {
        let storage = synced_store(vec![synced(
            "p.a",
            CollectionKind::Playlist,
            "Mine",
            &["i.1", "i.2", "i.3"],
        )])
        .await;
        // Cider's first page of the same playlist, plus a stray track.
        storage
            .apply_catalog_update(CatalogUpdate {
                collections: vec![Playlist::minimal("p.a", CollectionKind::Playlist, "Mine")],
                track_lists: vec![TrackListUpdate {
                    playlist_id: "p.a".to_owned(),
                    track_ids: vec!["i.9".to_owned()],
                    offset: 0,
                    complete: true,
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(contents.collections[0].track_ids, ["i.1", "i.2", "i.3"]);
    }

    #[tokio::test]
    async fn apple_music_should_order_playlists_accent_aware_then_albums() {
        let storage = synced_store(vec![
            synced(
                "l.b",
                CollectionKind::Album,
                "Example Album (Deluxe)",
                &["i.1"],
            ),
            synced("p.d", CollectionKind::Playlist, "Dinner 2024", &[]),
            synced("p.c", CollectionKind::Playlist, "Crème Brûlée", &[]),
            synced("p.cafe", CollectionKind::Playlist, "CAFE", &[]),
            synced("p.a", CollectionKind::Playlist, "Acoustic Morning ", &[]),
            synced("p.f", CollectionKind::Playlist, "Favourite Songs", &[]),
        ])
        .await;
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(
            names(&contents),
            [
                "Favourite Songs",
                "Acoustic Morning",
                "CAFE",
                "Crème Brûlée",
                "Dinner 2024",
                "Example Album (Deluxe)"
            ]
        );
    }

    #[tokio::test]
    async fn apple_music_should_not_synthesize_albums_from_songs() {
        let storage = synced_store(vec![synced(
            "p.a",
            CollectionKind::Playlist,
            "Mine",
            &["i.1"],
        )])
        .await;
        let contents = storage
            .library_contents(LibraryFilter::AppleMusic)
            .await
            .unwrap();
        assert_eq!(contents.collections.len(), 1);
    }

    #[tokio::test]
    async fn all_view_should_include_synced_collections() {
        let storage = synced_store(vec![synced(
            "p.a",
            CollectionKind::Playlist,
            "Mine",
            &["i.1"],
        )])
        .await;
        let contents = storage.library_contents(LibraryFilter::All).await.unwrap();
        assert!(
            contents
                .collections
                .iter()
                .any(|c| c.collection.id == "p.a" && c.track_ids == ["i.1"])
        );
    }

    #[test]
    fn sort_fold_should_strip_czech_diacritics() {
        assert_eq!(sort_fold(" Crème Brûlée "), "creme brulee");
    }
}
