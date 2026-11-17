//! In-memory index of everything the offline responder can answer from: the
//! API-synced library (collections + ordered members), every stored track
//! resource, which tracks have local audio, and the storefront.
//!
//! Built from the store in one pass and cached by the responder until the
//! store's [`Storage::offline_fingerprint`] changes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization as _;
use unicode_normalization::char::is_combining_mark;

use crate::cache::STOREFRONT_SETTING;
use crate::config::SongsPage;
use crate::storage::{
    CollectionKind, Storage, StorageError, SyncedCollection, Track, TrackMapping,
};

/// Storefront used when none was ever observed or derivable.
pub(crate) const FALLBACK_STOREFRONT: &str = "us";

/// One track (song or music video) known locally.
#[derive(Debug, Clone)]
pub(crate) struct LocalTrack {
    /// Row id: library id (`i.…`, `a.…`) or catalog id.
    pub(crate) id: String,
    /// Catalog (adam) id, when known.
    pub(crate) catalog_id: Option<String>,
    /// The stored resource without `relationships` (id, type, href, attributes).
    pub(crate) resource: Map<String, Value>,
    /// The catalog twin resource (`songs`), stored or synthesized.
    pub(crate) catalog: Option<Map<String, Value>>,
    /// Whether a local audio file exists.
    pub(crate) downloaded: bool,
    /// Whether the song is a saved library member (not merely in a playlist).
    pub(crate) in_library: bool,
    /// Catalog album id parsed from the song URL.
    pub(crate) catalog_album_id: Option<String>,
    /// The synced library album containing this track, if any.
    pub(crate) library_album: Option<usize>,
    /// Folded title / artist / album for matching.
    pub(crate) title_fold: String,
    pub(crate) artist_fold: String,
    pub(crate) album_fold: String,
    /// Display names.
    pub(crate) artist: String,
    pub(crate) album: Option<String>,
    /// Disc / track numbers for album ordering.
    pub(crate) disc: u64,
    pub(crate) number: u64,
}

impl LocalTrack {
    /// JSON `type` of the stored resource.
    pub(crate) fn kind(&self) -> &str {
        self.resource
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("songs")
    }

    /// Whether the stored resource is a library resource (`library-*`).
    pub(crate) fn is_library_resource(&self) -> bool {
        self.kind().starts_with("library-")
    }
}

/// A synced library playlist or album.
#[derive(Debug, Clone)]
pub(crate) struct LocalCollection {
    /// Library id (`p.…` / `l.…`).
    pub(crate) id: String,
    pub(crate) kind: CollectionKind,
    /// The stored library resource without `relationships`.
    pub(crate) resource: Map<String, Value>,
    /// The stored `relationships.catalog`, if Apple returned one.
    pub(crate) catalog_relationship: Option<Value>,
    /// Ordered member tracks (indexes into [`LocalIndex::tracks`]).
    pub(crate) tracks: Vec<usize>,
    /// Catalog playlist id (`playParams.globalId`, `pl.u-…`).
    pub(crate) global_id: Option<String>,
    /// Catalog album id (most common album of the member tracks).
    pub(crate) catalog_album_id: Option<String>,
    pub(crate) name_fold: String,
    pub(crate) artist_fold: String,
    /// `dateAdded` (ISO string, sorts chronologically).
    pub(crate) date_added: String,
    /// `lastModifiedDate`, falling back to `dateAdded`.
    pub(crate) last_modified: String,
}

impl LocalCollection {
    /// Library resource type (`library-playlists` / `library-albums`).
    pub(crate) fn library_type(&self) -> &'static str {
        match self.kind {
            CollectionKind::Album => "library-albums",
            CollectionKind::Playlist => "library-playlists",
        }
    }
}

/// A library artist derived from saved albums and songs.
#[derive(Debug, Clone)]
pub(crate) struct LocalArtist {
    /// Synthetic stable library id (`r.local-<hash>`).
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) name_fold: String,
    /// Library albums by this artist.
    pub(crate) albums: Vec<usize>,
    /// Saved songs by this artist.
    pub(crate) songs: Vec<usize>,
}

/// The offline index (see the [module docs](self)).
#[derive(Debug, Default)]
pub(crate) struct LocalIndex {
    pub(crate) storefront: String,
    /// The saved storefront document, if one was captured.
    pub(crate) storefront_document: Option<Value>,
    pub(crate) tracks: Vec<LocalTrack>,
    /// Synced collections in Apple's order (playlists, then albums).
    pub(crate) collections: Vec<LocalCollection>,
    pub(crate) artists: Vec<LocalArtist>,
    track_ids: HashMap<String, usize>,
    collection_ids: HashMap<String, usize>,
    artist_ids: HashMap<String, usize>,
    /// Catalog album id → member tracks (album order).
    catalog_albums: HashMap<String, Vec<usize>>,
    /// Apple's library Songs tab as synced, in `sort=dateAdded` order.
    songs_tab: Vec<usize>,
}

impl LocalIndex {
    /// Loads and indexes the store.
    pub(crate) async fn load(storage: &Storage) -> Result<Self, StorageError> {
        let synced = storage.synced_library().await?;
        let songs = storage.synced_library_songs().await?;
        let tracks = storage.list_tracks().await?;
        let mappings = storage.list_track_mappings().await?;
        let storefront = storage.get_setting(STOREFRONT_SETTING).await?;
        let index = tokio::task::spawn_blocking(move || {
            build(&synced, &songs, tracks, &mappings, storefront.as_deref())
        })
        .await?;
        Ok(index)
    }

    /// A track by row id, catalog id or library id.
    pub(crate) fn track(&self, id: &str) -> Option<&LocalTrack> {
        self.track_ids.get(id).map(|&i| &self.tracks[i])
    }

    /// A collection by library id, catalog playlist id or catalog album id.
    pub(crate) fn collection(&self, id: &str) -> Option<&LocalCollection> {
        self.collection_ids.get(id).map(|&i| &self.collections[i])
    }

    /// A library artist by its synthetic id.
    pub(crate) fn artist(&self, id: &str) -> Option<&LocalArtist> {
        self.artist_ids.get(id).map(|&i| &self.artists[i])
    }

    /// Tracks of a catalog album (album order), from every stored song.
    pub(crate) fn catalog_album_tracks(&self, album_id: &str) -> Option<&[usize]> {
        self.catalog_albums.get(album_id).map(Vec::as_slice)
    }

    /// Every catalog album id with at least one local track.
    pub(crate) fn catalog_album_ids(&self) -> impl Iterator<Item = &String> {
        self.catalog_albums.keys()
    }

    /// Playlists in Apple's order.
    pub(crate) fn playlists(&self) -> impl Iterator<Item = &LocalCollection> {
        self.collections
            .iter()
            .filter(|c| c.kind == CollectionKind::Playlist)
    }

    /// Albums in Apple's order.
    pub(crate) fn albums(&self) -> impl Iterator<Item = &LocalCollection> {
        self.collections
            .iter()
            .filter(|c| c.kind == CollectionKind::Album)
    }

    /// The songs the library Songs page lists for `page`, in its default
    /// order: Apple's Songs tab as synced (`sort=dateAdded` order), or every
    /// saved library song ([`Self::library_songs`]).
    pub(crate) fn songs_page(&self, page: SongsPage) -> Vec<usize> {
        match page {
            SongsPage::Apple => self.songs_tab.clone(),
            SongsPage::All => self.library_songs(),
        }
    }

    /// Saved library songs (library album members and other saved songs), in
    /// a stable order: library albums first (album order), then the rest.
    pub(crate) fn library_songs(&self) -> Vec<usize> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for album in self.albums() {
            for &t in &album.tracks {
                if seen.insert(t) {
                    out.push(t);
                }
            }
        }
        for (i, track) in self.tracks.iter().enumerate() {
            if track.in_library && seen.insert(i) {
                out.push(i);
            }
        }
        out
    }
}

/// Lower-cased, accent-free, punctuation-free form used for matching
/// (`Crème Brûlée` → `creme brulee`).
pub(crate) fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for c in text.nfkd().filter(|c| !is_combining_mark(*c)) {
        let c = match c {
            'ł' | 'Ł' => 'l',
            'ø' | 'Ø' => 'o',
            'đ' | 'Đ' => 'd',
            'ß' => 's',
            other => other,
        };
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn without_relationships(value: &Value) -> Option<Map<String, Value>> {
    let mut object = value.as_object()?.clone();
    object.remove("relationships");
    Some(object)
}

/// The album id of a catalog song URL (`…/album/<slug>/<id>?i=…`).
pub(crate) fn album_id_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next()?;
    let mut parts = path.rsplit('/');
    let id = parts.next()?;
    let _slug = parts.next()?;
    (parts.next() == Some("album") && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.to_owned())
}

/// The storefront segment of a stored `/v1/catalog/{sf}/…` href.
fn storefront_from(value: &Value) -> Option<String> {
    let href =
        str_at(value, "/relationships/catalog/data/0/href").or_else(|| str_at(value, "/href"))?;
    let rest = href.strip_prefix("/v1/catalog/")?;
    let sf = rest.split('/').next()?;
    (sf.len() == 2).then(|| sf.to_owned())
}

/// Stable synthetic library-artist id for `name`.
fn artist_id(name_fold: &str) -> String {
    // FNV-1a: stable across runs (unlike `DefaultHasher`).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name_fold.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("r.local-{hash:016x}")
}

fn file_exists(path: &Path) -> bool {
    path.is_file()
}

/// Synthesizes a catalog `songs` resource from a library-song resource that
/// carries no catalog relationship.
fn synthesized_catalog(resource: &Value, catalog_id: &str, storefront: &str) -> Map<String, Value> {
    let mut attributes = resource
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    attributes.insert(
        "playParams".to_owned(),
        serde_json::json!({"id": catalog_id, "kind": "song"}),
    );
    if !attributes.contains_key("url") {
        let name = attributes
            .get("name")
            .and_then(Value::as_str)
            .map(fold)
            .unwrap_or_default()
            .replace(' ', "-");
        attributes.insert(
            "url".to_owned(),
            Value::from(format!(
                "https://music.apple.com/{storefront}/song/{name}/{catalog_id}"
            )),
        );
    }
    let mut out = Map::new();
    out.insert("id".to_owned(), Value::from(catalog_id));
    out.insert("type".to_owned(), Value::from("songs"));
    out.insert(
        "href".to_owned(),
        Value::from(format!("/v1/catalog/{storefront}/songs/{catalog_id}")),
    );
    out.insert("attributes".to_owned(), Value::Object(attributes));
    out
}

#[expect(clippy::too_many_lines, reason = "one linear indexing pass")]
fn build(
    synced: &[SyncedCollection],
    songs_tab: &[String],
    tracks: Vec<Track>,
    mappings: &[TrackMapping],
    storefront_setting: Option<&str>,
) -> LocalIndex {
    let mapped: HashSet<&str> = mappings
        .iter()
        .filter(|m| file_exists(&m.file_path))
        .map(|m| m.track_id.as_str())
        .collect();

    let storefront_document: Option<Value> =
        storefront_setting.and_then(|s| serde_json::from_str(s).ok());
    let mut storefront = storefront_document
        .as_ref()
        .and_then(|d| str_at(d, "/data/0/id"))
        .map(str::to_owned);

    let mut index = LocalIndex::default();
    let mut parsed: Vec<(Track, Value)> = tracks
        .into_iter()
        .filter(|t| !t.is_custom)
        .filter_map(|t| {
            let value = t
                .metadata
                .as_deref()
                .and_then(|m| serde_json::from_str::<Value>(m).ok())?;
            value.get("id")?;
            Some((t, value))
        })
        .collect();
    if storefront.is_none() {
        storefront = parsed.iter().find_map(|(_, v)| storefront_from(v));
    }
    let storefront = storefront.unwrap_or_else(|| FALLBACK_STOREFRONT.to_owned());

    // Downloaded rows first, so an id shared by two rows resolves to the playable one.
    parsed.sort_by_key(|(t, _)| {
        let downloaded = t.audio_path.as_deref().is_some_and(file_exists);
        !downloaded
    });
    for (track, value) in parsed {
        let downloaded = track.audio_path.as_deref().is_some_and(file_exists)
            || mapped.contains(track.id.as_str())
            || track
                .catalog_id
                .as_deref()
                .is_some_and(|c| mapped.contains(c));
        let catalog = value
            .pointer("/relationships/catalog/data/0")
            .and_then(without_relationships)
            .or_else(|| {
                let is_library = str_at(&value, "/type").is_some_and(|t| t.starts_with("library-"));
                match (&track.catalog_id, is_library) {
                    (Some(cid), true) => Some(synthesized_catalog(&value, cid, &storefront)),
                    (_, false) => without_relationships(&value),
                    (None, true) => None,
                }
            });
        // Covers are cached for the library resource's artwork (the sync
        // prefetches it); a catalog twin with a different image (music
        // videos) shows the cached one instead of an uncached cover.
        let mut catalog = catalog;
        let library_artwork = str_at(&value, "/attributes/artwork/url")
            .filter(|_| str_at(&value, "/type").is_some_and(|t| t.starts_with("library-")));
        if let (Some(catalog), Some(library_url)) = (catalog.as_mut(), library_artwork)
            && let Some(artwork) = catalog
                .get_mut("attributes")
                .and_then(|a| a.get_mut("artwork"))
                .and_then(Value::as_object_mut)
            && artwork.get("url").and_then(Value::as_str) != Some(library_url)
        {
            artwork.insert("url".to_owned(), Value::from(library_url));
        }
        if let Some(attributes) = catalog
            .as_mut()
            .and_then(|c| c.get_mut("attributes"))
            .and_then(Value::as_object_mut)
        {
            attributes.remove("previews");
        }
        let url = catalog
            .as_ref()
            .and_then(|c| c.get("attributes"))
            .and_then(|a| a.get("url"))
            .and_then(Value::as_str);
        let catalog_album_id = url.and_then(album_id_from_url);
        let attributes = value.get("attributes");
        let number = |key: &str| {
            attributes
                .and_then(|a| a.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let Some(mut resource) = without_relationships(&value) else {
            continue;
        };
        // Attributes Apple puts on both twins but the stored library copy
        // may lack (it was fetched without them).
        if let (Some(attributes), Some(catalog_attributes)) = (
            resource
                .get_mut("attributes")
                .and_then(Value::as_object_mut),
            catalog
                .as_ref()
                .and_then(|c| c.get("attributes"))
                .and_then(Value::as_object),
        ) {
            // Preview clips cannot play offline (and carry uncached art).
            attributes.remove("previews");
            for key in ["contentRating", "hasLyrics", "hasTimeSyncedLyrics"] {
                if !attributes.contains_key(key)
                    && let Some(value) = catalog_attributes.get(key)
                {
                    attributes.insert(key.to_owned(), value.clone());
                }
            }
        }
        let local = LocalTrack {
            in_library: track.in_library || track.id.starts_with("i."),
            title_fold: fold(&track.title),
            artist_fold: fold(&track.artist),
            album_fold: fold(track.album.as_deref().unwrap_or_default()),
            artist: track.artist.clone(),
            album: track.album.clone(),
            disc: number("discNumber"),
            number: number("trackNumber"),
            id: track.id.clone(),
            catalog_id: track.catalog_id.clone(),
            resource,
            catalog,
            downloaded,
            catalog_album_id,
            library_album: None,
        };
        let position = index.tracks.len();
        let mut keys = vec![local.id.clone()];
        if let Some(cid) = local
            .catalog
            .as_ref()
            .and_then(|c| c.get("id"))
            .and_then(Value::as_str)
        {
            keys.push(cid.to_owned());
        }
        for key in keys {
            index.track_ids.entry(key).or_insert(position);
        }
        index.tracks.push(local);
    }
    // A row's linked `catalog_id` (e.g. the playable twin of a library music
    // video) is only an alias: it never shadows a row that *is* that id.
    for (position, track) in index.tracks.iter().enumerate() {
        if let Some(cid) = &track.catalog_id {
            index.track_ids.entry(cid.clone()).or_insert(position);
        }
    }
    // Catalog-only rows show the cover cached for their library twin.
    let library_artwork: HashMap<String, String> = index
        .tracks
        .iter()
        .filter(|t| t.is_library_resource())
        .filter_map(|t| {
            let url = t
                .resource
                .get("attributes")?
                .get("artwork")?
                .get("url")?
                .as_str()?;
            Some((t.catalog_id.clone()?, url.to_owned()))
        })
        .collect();
    for track in &mut index.tracks {
        let Some(catalog) = track.catalog.as_mut() else {
            continue;
        };
        let Some(url) = catalog
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| library_artwork.get(id))
        else {
            continue;
        };
        if let Some(artwork) = catalog
            .get_mut("attributes")
            .and_then(|a| a.get_mut("artwork"))
            .and_then(Value::as_object_mut)
        {
            artwork.insert("url".to_owned(), Value::from(url.clone()));
        }
    }

    for synced_collection in synced {
        let collection = &synced_collection.collection;
        let Some(value) = collection
            .metadata
            .as_deref()
            .and_then(|m| serde_json::from_str::<Value>(m).ok())
        else {
            continue;
        };
        let Some(resource) = without_relationships(&value) else {
            continue;
        };
        let members: Vec<usize> = synced_collection
            .track_ids
            .iter()
            .filter_map(|id| index.track_ids.get(id).copied())
            .collect();
        let attr = |key: &str| str_at(&value, &format!("/attributes/{key}")).map(str::to_owned);
        let date_added = attr("dateAdded").unwrap_or_default();
        let position = index.collections.len();
        if collection.kind == CollectionKind::Album {
            for &t in &members {
                let track = &mut index.tracks[t];
                track.library_album.get_or_insert(position);
                track.in_library = true;
            }
        }
        let mut album_counts: HashMap<&str, usize> = HashMap::new();
        for &t in &members {
            if let Some(album) = index.tracks[t].catalog_album_id.as_deref() {
                *album_counts.entry(album).or_default() += 1;
            }
        }
        let catalog_album_id = (collection.kind == CollectionKind::Album)
            .then(|| {
                album_counts
                    .into_iter()
                    .max_by_key(|(_, n)| *n)
                    .map(|(id, _)| id.to_owned())
            })
            .flatten();
        let local = LocalCollection {
            id: collection.id.clone(),
            kind: collection.kind,
            catalog_relationship: value.pointer("/relationships/catalog").cloned(),
            tracks: members,
            global_id: str_at(&value, "/attributes/playParams/globalId").map(str::to_owned),
            catalog_album_id,
            name_fold: fold(&collection.name),
            artist_fold: fold(collection.artist.as_deref().unwrap_or_default()),
            last_modified: attr("lastModifiedDate").unwrap_or_else(|| date_added.clone()),
            date_added,
            resource,
        };
        index
            .collection_ids
            .entry(local.id.clone())
            .or_insert(position);
        if let Some(global) = &local.global_id {
            index
                .collection_ids
                .entry(global.clone())
                .or_insert(position);
        }
        if let Some(album) = &local.catalog_album_id {
            index
                .collection_ids
                .entry(album.clone())
                .or_insert(position);
        }
        index.collections.push(local);
    }

    // The Songs tab is saved library membership by definition.
    index.songs_tab = songs_tab
        .iter()
        .filter_map(|id| index.track_ids.get(id).copied())
        .collect();
    for &t in &index.songs_tab {
        index.tracks[t].in_library = true;
    }

    let mut albums: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, track) in index.tracks.iter().enumerate() {
        if let Some(album) = &track.catalog_album_id {
            albums.entry(album.clone()).or_default().push(i);
        }
    }
    for members in albums.values_mut() {
        members.sort_by_key(|&i| (index.tracks[i].disc, index.tracks[i].number));
    }
    index.catalog_albums = albums;

    // Library artists: artists of saved albums and saved songs, by name.
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut artist_of = |index: &mut LocalIndex, name: &str| -> Option<usize> {
        let name_fold = fold(name);
        if name_fold.is_empty() {
            return None;
        }
        Some(*by_name.entry(name_fold.clone()).or_insert_with(|| {
            index.artists.push(LocalArtist {
                id: artist_id(&name_fold),
                name: name.to_owned(),
                name_fold,
                albums: Vec::new(),
                songs: Vec::new(),
            });
            index.artists.len() - 1
        }))
    };
    let album_artists: Vec<(usize, String)> = index
        .collections
        .iter()
        .enumerate()
        .filter(|(_, c)| c.kind == CollectionKind::Album)
        .filter_map(|(i, c)| {
            str_at(&Value::Object(c.resource.clone()), "/attributes/artistName")
                .map(|a| (i, a.to_owned()))
        })
        .collect();
    for (album, name) in album_artists {
        if let Some(a) = artist_of(&mut index, &name) {
            index.artists[a].albums.push(album);
        }
    }
    for song in index.library_songs() {
        let name = index.tracks[song].artist.clone();
        if let Some(a) = artist_of(&mut index, &name) {
            index.artists[a].songs.push(song);
        }
    }
    index.artists.sort_by(|a, b| a.name_fold.cmp(&b.name_fold));
    index.artist_ids = index
        .artists
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.clone(), i))
        .collect();

    index.storefront = storefront;
    index.storefront_document = storefront_document;
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_should_drop_accents_case_and_punctuation() {
        assert_eq!(fold("  Crème   Brûlée! (Remix) "), "creme brulee remix");
    }

    #[test]
    fn album_id_should_parse_from_song_url() {
        assert_eq!(
            album_id_from_url("https://music.apple.com/cz/album/freedom/1010891982?i=1010892001")
                .as_deref(),
            Some("1010891982")
        );
    }

    #[test]
    fn artist_id_should_be_stable() {
        assert_eq!(artist_id("example artist"), artist_id("example artist"));
    }
}
