//! Library injection: makes user-created custom albums, playlists and songs
//! appear inside Cider's library — in search, in the library listings, in
//! single-resource and `?ids=` lookups, and in collection track pages — online
//! and offline through one code path.
//!
//! Custom items use reserved ids (`local-<n>` songs, `local-al-<n>` albums,
//! `local-pl-<n>` playlists). Playing one makes Cider request `WebPlayback`
//! for that id, which [`crate::playback::LocalPlaybackInterceptor`] resolves to
//! a local file.
//!
//! The merge logic is shared by two callers so fake items surface identically
//! whether Apple is reachable or not:
//!
//! - online: [`LibraryInjectionInterceptor`] runs in the proxy pipeline's
//!   pre-cache slot, merging into the real upstream response (and short-circuits
//!   a lookup whose ids are all custom, which upstream would only 404);
//! - offline: [`crate::offline`] calls [`inject_into`] on the document it has
//!   produced (from cache or synthesis) before answering.

use std::collections::HashSet;

use bytes::Bytes;
use hudsucker::Body;
use hudsucker::hyper::{Method, Request, StatusCode, Uri};
use serde_json::{Map, Value, json};

use crate::apple::{self, ResourceLookup, SearchKind};
use crate::interceptor_prelude::*;
use crate::respond::{self, buffer_decoded, content_type_essence};
use crate::storage::{CUSTOM_ID_PREFIX, CollectionKind, Playlist, Storage, StorageError, Track};

/// Most custom songs injected into one search response.
const MAX_INJECTED: usize = 25;

/// What a request path lets us inject into.
#[derive(Debug, Clone)]
enum Target {
    /// `…/search`: custom songs matching `term`.
    Search(SearchKind),
    /// `/v1/me/library/{songs|albums|playlists}` listing (no id, no `ids=`).
    Listing(LibraryKind),
    /// A single-resource or `?ids=` lookup of songs / albums / playlists.
    Lookup {
        /// Catalog or library flavour (from the path).
        flavor: SearchKind,
        /// Parsed ids + kind.
        lookup: ResourceLookup,
    },
    /// `…/{playlists|albums}/{id}/tracks`.
    CollectionTracks {
        /// Catalog or library flavour (from the path).
        flavor: SearchKind,
        /// Collection id.
        id: String,
    },
}

/// Which library listing a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryKind {
    Songs,
    Albums,
    Playlists,
}

/// Classifies a request into an injection [`Target`] (pure; no storage).
fn target(method: &Method, uri: &Uri) -> Option<Target> {
    if *method != Method::GET {
        return None;
    }
    let path = uri.path();
    if let Some(kind) = apple::search_kind(path) {
        return Some(Target::Search(kind));
    }
    // A single-resource or `?ids=` lookup (covers `/{kind}/{id}` and `/{kind}?ids=`).
    if let Some(lookup) = apple::resource_lookup(uri) {
        return Some(Target::Lookup {
            flavor: flavor_of(path),
            lookup,
        });
    }
    if let Some(kind) = library_listing_kind(path) {
        return Some(Target::Listing(kind));
    }
    if let Some(id) = apple::collection_tracks_target(path) {
        return Some(Target::CollectionTracks {
            flavor: flavor_of(path),
            id,
        });
    }
    None
}

/// Parses `/v1/me/library/{songs|albums|playlists}` (the bare listing).
fn library_listing_kind(path: &str) -> Option<LibraryKind> {
    let parts: Vec<&str> = apple::segments(path).collect();
    match parts.as_slice() {
        ["v1", "me", "library", "songs"] => Some(LibraryKind::Songs),
        ["v1", "me", "library", "albums"] => Some(LibraryKind::Albums),
        ["v1", "me", "library", "playlists"] => Some(LibraryKind::Playlists),
        _ => None,
    }
}

/// Catalog vs library flavour, by whether the path is a `/v1/catalog/…` one.
fn flavor_of(path: &str) -> SearchKind {
    if apple::segments(path).nth(1) == Some("catalog") {
        SearchKind::Catalog
    } else {
        SearchKind::Library
    }
}

/// Storefront segment of a catalog path (`/v1/catalog/{sf}/…`), else `"us"`.
fn storefront_of(path: &str) -> String {
    apple::segments(path).nth(2).unwrap_or("us").to_owned()
}

/// Whether `id` is a custom (user-created) item id.
fn is_custom_id(id: &str) -> bool {
    id.starts_with(CUSTOM_ID_PREFIX)
}

/// Whether the request can be answered entirely from custom items, so it should
/// be served locally instead of sent upstream (which would only 404).
fn is_local_only(target: &Target) -> bool {
    match target {
        Target::Lookup { lookup, .. } => lookup.ids.iter().all(|id| is_custom_id(id)),
        Target::CollectionTracks { id, .. } => is_custom_id(id),
        Target::Search(_) | Target::Listing(_) => false,
    }
}

// ----- Public entry points ----------------------------------------------------

/// Merges custom items into `document` for the request `uri`. Returns whether
/// the document was changed. Used by [`crate::offline`] for the offline path.
///
/// # Errors
///
/// Returns storage errors from enumerating custom items.
pub(crate) async fn inject_into(
    storage: &Storage,
    method: &Method,
    uri: &Uri,
    document: &mut Value,
) -> Result<bool, StorageError> {
    let Some(target) = target(method, uri) else {
        return Ok(false);
    };
    apply(storage, uri, &target, document).await
}

/// Builds the document that answers a fully-custom request locally, or `None`
/// when the request is not one the injector should answer on its own.
async fn local_document(
    storage: &Storage,
    method: &Method,
    uri: &Uri,
) -> Result<Option<Value>, StorageError> {
    let Some(target) = target(method, uri) else {
        return Ok(None);
    };
    if !is_local_only(&target) {
        return Ok(None);
    }
    let mut document = json!({ "data": [] });
    apply(storage, uri, &target, &mut document).await?;
    Ok(Some(document))
}

// ----- Merge dispatch ---------------------------------------------------------

async fn apply(
    storage: &Storage,
    uri: &Uri,
    target: &Target,
    document: &mut Value,
) -> Result<bool, StorageError> {
    match target {
        Target::Search(kind) => merge_search(storage, uri, *kind, document).await,
        Target::Listing(kind) => merge_listing(storage, uri, *kind, document).await,
        Target::Lookup { flavor, lookup } => {
            merge_lookup(storage, uri, *flavor, lookup, document).await
        }
        Target::CollectionTracks { flavor, id } => {
            merge_collection_tracks(storage, uri, *flavor, id, document).await
        }
    }
}

// ----- Search -----------------------------------------------------------------

/// Whether every whitespace-separated word of `term` occurs in the track's
/// title, artist or album (case-insensitive).
fn matches(track: &Track, term: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        track.title,
        track.artist,
        track.album.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    let mut words = term.split_whitespace().peekable();
    words.peek().is_some() && words.all(|word| haystack.contains(&word.to_lowercase()))
}

/// Whether the request's `types` parameter (if any) asks for songs.
fn wants_songs(types: Option<&str>, kind: SearchKind) -> bool {
    types.is_none_or(|types| {
        types
            .split(',')
            .any(|t| t.trim() == kind.song_resource_type())
    })
}

async fn merge_search(
    storage: &Storage,
    uri: &Uri,
    kind: SearchKind,
    document: &mut Value,
) -> Result<bool, StorageError> {
    let types = apple::query_param(uri, "types");
    let Some(term) = apple::query_param(uri, "term") else {
        return Ok(false);
    };
    if !wants_songs(types.as_deref(), kind) {
        return Ok(false);
    }
    let storefront = storefront_of(uri.path());
    let songs: Vec<Value> = storage
        .list_custom_tracks()
        .await?
        .iter()
        .filter(|track| matches(track, &term))
        .take(MAX_INJECTED)
        .map(|track| song_resource(track, kind, &storefront))
        .collect();
    if songs.is_empty() {
        return Ok(false);
    }
    inject_search_group(document, kind, songs);
    Ok(true)
}

/// Prepends `songs` to `results[<songs key>].data`, creating the group as
/// needed and listing a newly created catalog group in `meta.results.order`.
fn inject_search_group(document: &mut Value, kind: SearchKind, songs: Vec<Value>) {
    let Some(doc) = ensure_object(document) else {
        return;
    };
    let key = kind.songs_key();
    let results = doc.entry("results").or_insert_with(|| json!({}));
    let Some(results) = results.as_object_mut() else {
        return;
    };
    let newly_created = !results.contains_key(key);
    let group = results.entry(key).or_insert_with(|| match kind {
        SearchKind::Catalog => json!({"groupId": key, "name": "Songs", "data": []}),
        SearchKind::Library => json!({"data": []}),
    });
    if let Some(group) = group.as_object_mut() {
        let data = group.entry("data").or_insert_with(|| json!([]));
        if let Some(data) = data.as_array_mut() {
            data.splice(0..0, songs);
        }
    }
    if newly_created && kind == SearchKind::Catalog {
        ensure_meta_order(doc, key);
    }
}

/// Ensures `key` appears in `meta.results.order` and `meta.results.rawOrder`.
fn ensure_meta_order(doc: &mut Map<String, Value>, key: &str) {
    let meta = doc.entry("meta").or_insert_with(|| json!({}));
    let Some(meta) = meta.as_object_mut() else {
        return;
    };
    let results = meta.entry("results").or_insert_with(|| json!({}));
    let Some(results) = results.as_object_mut() else {
        return;
    };
    for field in ["order", "rawOrder"] {
        let list = results.entry(field).or_insert_with(|| json!([]));
        if let Some(list) = list.as_array_mut()
            && !list.iter().any(|v| v.as_str() == Some(key))
        {
            list.push(Value::String(key.to_owned()));
        }
    }
}

// ----- Library listings -------------------------------------------------------

async fn merge_listing(
    storage: &Storage,
    uri: &Uri,
    kind: LibraryKind,
    document: &mut Value,
) -> Result<bool, StorageError> {
    // Only augment the first page, or the prepended items would repeat on every
    // page and inflate paging.
    let offset = apple::query_param(uri, "offset").and_then(|o| o.parse::<u64>().ok());
    if offset.is_some_and(|o| o > 0) {
        return Ok(false);
    }
    let items = match kind {
        LibraryKind::Songs => storage
            .list_custom_tracks()
            .await?
            .iter()
            .map(|track| song_resource(track, SearchKind::Library, ""))
            .collect(),
        LibraryKind::Albums => listing_albums(storage).await?,
        LibraryKind::Playlists => {
            let mut out = Vec::new();
            for collection in storage.list_custom_collections().await? {
                if collection.kind == CollectionKind::Playlist {
                    let count = storage.playlist_track_ids(&collection.id).await?.len();
                    out.push(playlist_resource(
                        &collection,
                        SearchKind::Library,
                        "",
                        None,
                        count,
                    ));
                }
            }
            out
        }
    };
    let count = items.len();
    if !prepend_unique(document, items) {
        return Ok(false);
    }
    bump_total(document, count);
    Ok(true)
}

/// Library-album resources to inject into `/v1/me/library/albums`: every custom
/// album, plus every distinct real album a custom song is tagged into for which
/// a genuine resource is cached (so we never fabricate a real album).
async fn listing_albums(storage: &Storage) -> Result<Vec<Value>, StorageError> {
    let mut out = Vec::new();
    for collection in storage.list_custom_collections().await? {
        if collection.kind == CollectionKind::Album {
            let count = storage.playlist_track_ids(&collection.id).await?.len();
            out.push(album_resource(
                &collection,
                SearchKind::Library,
                "",
                None,
                count,
            ));
        }
    }
    let mut seen = HashSet::new();
    for track in storage.list_custom_tracks().await? {
        let Some(real_album_id) = &track.real_album_id else {
            continue;
        };
        if !seen.insert(real_album_id.clone()) {
            continue;
        }
        if let Some(resource) = storage
            .get_playlist(real_album_id)
            .await?
            .and_then(|album| parse_metadata(album.metadata.as_deref()))
        {
            out.push(resource);
        }
    }
    Ok(out)
}

// ----- Resource lookups -------------------------------------------------------

async fn merge_lookup(
    storage: &Storage,
    uri: &Uri,
    flavor: SearchKind,
    lookup: &ResourceLookup,
    document: &mut Value,
) -> Result<bool, StorageError> {
    let storefront = storefront_of(uri.path());
    let mut custom = Vec::new();
    for id in &lookup.ids {
        if !is_custom_id(id) {
            continue;
        }
        match lookup.kind.as_str() {
            "songs" => {
                if let Some(track) = custom_track(storage, id).await? {
                    custom.push(song_resource(&track, flavor, &storefront));
                }
            }
            "albums" | "playlists" => {
                if let Some(resource) =
                    custom_collection_resource(storage, id, flavor, &storefront).await?
                {
                    custom.push(resource);
                }
            }
            _ => {}
        }
    }
    let mut modified = prepend_unique(document, custom);

    // A custom song tagged into a real album is injected as an extra track of
    // that real album, with the whole track list renumbered so the numbering
    // stays consistent (Open Question (a), option A2).
    if lookup.kind == "albums" {
        for id in &lookup.ids {
            if is_custom_id(id) {
                continue;
            }
            let tagged = storage.tracks_in_real_album(id).await?;
            if !tagged.is_empty()
                && inject_real_album_tracks(document, id, &tagged, flavor, &storefront)
            {
                modified = true;
            }
        }
    }
    Ok(modified)
}

/// A custom song track by id, or `None` if it is missing or not custom.
async fn custom_track(storage: &Storage, id: &str) -> Result<Option<Track>, StorageError> {
    Ok(storage.get_track(id).await?.filter(|track| track.is_custom))
}

/// A full custom album / playlist resource, with its ordered member songs.
async fn custom_collection_resource(
    storage: &Storage,
    id: &str,
    flavor: SearchKind,
    storefront: &str,
) -> Result<Option<Value>, StorageError> {
    let Some(collection) = storage
        .get_playlist(id)
        .await?
        .filter(|collection| collection.is_custom)
    else {
        return Ok(None);
    };
    let members = collection_member_resources(storage, id, flavor, storefront).await?;
    let count = members.len();
    Ok(Some(match collection.kind {
        CollectionKind::Album => {
            album_resource(&collection, flavor, storefront, Some(members), count)
        }
        CollectionKind::Playlist => {
            playlist_resource(&collection, flavor, storefront, Some(members), count)
        }
    }))
}

// ----- Collection track pages -------------------------------------------------

async fn merge_collection_tracks(
    storage: &Storage,
    uri: &Uri,
    flavor: SearchKind,
    id: &str,
    document: &mut Value,
) -> Result<bool, StorageError> {
    let storefront = storefront_of(uri.path());
    if is_custom_id(id) {
        // A custom collection's whole track page is its ordered members.
        let members = collection_member_resources(storage, id, flavor, &storefront).await?;
        if members.is_empty() {
            return Ok(false);
        }
        set_data(document, members);
        return Ok(true);
    }
    // Real album: splice in the custom songs tagged into it, renumbering.
    let tagged = storage.tracks_in_real_album(id).await?;
    if tagged.is_empty() {
        return Ok(false);
    }
    let customs: Vec<(usize, Value)> = tagged
        .iter()
        .map(|track| {
            (
                track_index(track),
                song_resource(track, flavor, &storefront),
            )
        })
        .collect();
    let Some(data) = data_array(document) else {
        return Ok(false);
    };
    splice_and_renumber(data, customs);
    Ok(true)
}

/// Resources for a collection's ordered members (`position` → `trackNumber`).
async fn collection_member_resources(
    storage: &Storage,
    collection_id: &str,
    flavor: SearchKind,
    storefront: &str,
) -> Result<Vec<Value>, StorageError> {
    let ids = storage.playlist_track_ids(collection_id).await?;
    let mut resources = Vec::with_capacity(ids.len());
    for (position, track_id) in ids.iter().enumerate() {
        let Some(mut track) = custom_track(storage, track_id).await? else {
            continue;
        };
        // Membership position is authoritative for numbering within a collection.
        track.track_number = u32::try_from(position + 1).ok();
        resources.push(song_resource(&track, flavor, storefront));
    }
    Ok(resources)
}

// ----- Real-album track injection ---------------------------------------------

/// Injects the tagged songs into the album resource identified by `album_id`
/// in `document.data`, rewriting the track order + `trackCount`. Returns
/// whether the album was found and changed.
fn inject_real_album_tracks(
    document: &mut Value,
    album_id: &str,
    tagged: &[Track],
    flavor: SearchKind,
    storefront: &str,
) -> bool {
    let Some(data) = data_array(document) else {
        return false;
    };
    let Some(album) = data
        .iter_mut()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(album_id))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let customs: Vec<(usize, Value)> = tagged
        .iter()
        .map(|track| (track_index(track), song_resource(track, flavor, storefront)))
        .collect();

    let relationships = album
        .entry("relationships")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    let Some(relationships) = relationships else {
        return false;
    };
    let tracks = relationships
        .entry("tracks")
        .or_insert_with(|| json!({"data": []}));
    let Some(tracks) = tracks.as_object_mut() else {
        return false;
    };
    let data = tracks.entry("data").or_insert_with(|| json!([]));
    let Some(data) = data.as_array_mut() else {
        return false;
    };
    let total = splice_and_renumber(data, customs);
    // Keep the album's advertised track count consistent with the list.
    if let Some(attributes) = album
        .entry("attributes")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    {
        attributes.insert("trackCount".to_owned(), json!(total));
    }
    true
}

/// Inserts each `(index, resource)` into `data` at its clamped index (ascending
/// by index), then renumbers every item's `attributes.trackNumber` to its
/// 1-based position. Returns the new length.
fn splice_and_renumber(data: &mut Vec<Value>, mut customs: Vec<(usize, Value)>) -> usize {
    customs.sort_by_key(|(index, _)| *index);
    for (index, resource) in customs {
        let at = index.min(data.len());
        data.insert(at, resource);
    }
    for (position, item) in data.iter_mut().enumerate() {
        if let Some(attributes) = item.get_mut("attributes").and_then(Value::as_object_mut) {
            attributes.insert("trackNumber".to_owned(), json!(position + 1));
        }
    }
    data.len()
}

/// Zero-based insertion index a tagged track wants (from its 1-based
/// `track_number`), defaulting to the end when unset.
fn track_index(track: &Track) -> usize {
    track
        .track_number
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(usize::MAX)
}

// ----- Resource builders ------------------------------------------------------

/// Full Apple-shaped song resource for a custom song.
fn song_resource(track: &Track, flavor: SearchKind, storefront: &str) -> Value {
    let id = &track.id;
    let (resource_type, href, play_params) = match flavor {
        SearchKind::Catalog => (
            "songs",
            format!("/v1/catalog/{storefront}/songs/{id}"),
            json!({"id": id, "kind": "song"}),
        ),
        SearchKind::Library => (
            "library-songs",
            format!("/v1/me/library/songs/{id}"),
            json!({"id": id, "kind": "song", "isLibrary": true}),
        ),
    };
    let mut attributes = Map::new();
    attributes.insert("name".to_owned(), json!(track.title));
    attributes.insert("artistName".to_owned(), json!(track.artist));
    attributes.insert(
        "albumName".to_owned(),
        json!(track.album.clone().unwrap_or_default()),
    );
    insert_opt(
        &mut attributes,
        "albumArtistName",
        track.album_artist.as_ref(),
    );
    insert_opt(&mut attributes, "composerName", track.composer.as_ref());
    attributes.insert(
        "durationInMillis".to_owned(),
        json!(track.duration_ms.unwrap_or(0)),
    );
    attributes.insert(
        "trackNumber".to_owned(),
        json!(track.track_number.unwrap_or(1)),
    );
    attributes.insert(
        "discNumber".to_owned(),
        json!(track.disc_number.unwrap_or(1)),
    );
    attributes.insert("genreNames".to_owned(), genre_names(track.genre.as_deref()));
    insert_opt(&mut attributes, "releaseDate", track.release_date.as_ref());
    insert_opt(&mut attributes, "isrc", track.isrc.as_ref());
    insert_opt(&mut attributes, "url", track.url.as_ref());
    attributes.insert("hasLyrics".to_owned(), json!(track.has_lyrics));
    attributes.insert(
        "hasTimeSyncedLyrics".to_owned(),
        json!(track.has_time_synced_lyrics),
    );
    insert_opt(
        &mut attributes,
        "contentRating",
        track.content_rating.as_ref(),
    );
    if let Some(artwork) = artwork_value(track.artwork_url.as_deref()) {
        attributes.insert("artwork".to_owned(), artwork);
    }
    attributes.insert("playParams".to_owned(), play_params);
    json!({
        "id": id,
        "type": resource_type,
        "href": href,
        "attributes": Value::Object(attributes),
    })
}

/// Full Apple-shaped album resource for a custom album (or any [`Playlist`] row
/// of album kind), optionally carrying its track list.
fn album_resource(
    album: &Playlist,
    flavor: SearchKind,
    storefront: &str,
    tracks: Option<Vec<Value>>,
    track_count: usize,
) -> Value {
    let id = &album.id;
    let (resource_type, href, play_params) = match flavor {
        SearchKind::Catalog => (
            "albums",
            format!("/v1/catalog/{storefront}/albums/{id}"),
            json!({"id": id, "kind": "album"}),
        ),
        SearchKind::Library => (
            "library-albums",
            format!("/v1/me/library/albums/{id}"),
            json!({"id": id, "kind": "album", "isLibrary": true}),
        ),
    };
    let mut attributes = Map::new();
    attributes.insert("name".to_owned(), json!(album.name));
    attributes.insert(
        "artistName".to_owned(),
        json!(album.artist.clone().unwrap_or_default()),
    );
    attributes.insert("trackCount".to_owned(), json!(track_count));
    attributes.insert("genreNames".to_owned(), genre_names(album.genre.as_deref()));
    insert_opt(&mut attributes, "releaseDate", album.release_date.as_ref());
    if let Some(artwork) = artwork_value(album.artwork_url.as_deref()) {
        attributes.insert("artwork".to_owned(), artwork);
    }
    attributes.insert("playParams".to_owned(), play_params);
    let mut resource = json!({
        "id": id,
        "type": resource_type,
        "href": href,
        "attributes": Value::Object(attributes),
    });
    if let Some(tracks) = tracks
        && let Some(object) = resource.as_object_mut()
    {
        object.insert(
            "relationships".to_owned(),
            json!({"tracks": {"data": tracks}}),
        );
    }
    resource
}

/// Full Apple-shaped playlist resource for a custom playlist, optionally
/// carrying its track list.
fn playlist_resource(
    playlist: &Playlist,
    flavor: SearchKind,
    storefront: &str,
    tracks: Option<Vec<Value>>,
    track_count: usize,
) -> Value {
    let id = &playlist.id;
    let (resource_type, href, play_params) = match flavor {
        SearchKind::Catalog => (
            "playlists",
            format!("/v1/catalog/{storefront}/playlists/{id}"),
            json!({"id": id, "kind": "playlist"}),
        ),
        SearchKind::Library => (
            "library-playlists",
            format!("/v1/me/library/playlists/{id}"),
            json!({"id": id, "kind": "playlist", "isLibrary": true}),
        ),
    };
    let mut attributes = Map::new();
    attributes.insert("name".to_owned(), json!(playlist.name));
    attributes.insert("trackCount".to_owned(), json!(track_count));
    attributes.insert("canEdit".to_owned(), json!(false));
    attributes.insert("hasCatalog".to_owned(), json!(false));
    attributes.insert("isPublic".to_owned(), json!(false));
    insert_opt(
        &mut attributes,
        "description",
        playlist.description.as_ref(),
    );
    if let Some(artwork) = artwork_value(playlist.artwork_url.as_deref()) {
        attributes.insert("artwork".to_owned(), artwork);
    }
    attributes.insert("playParams".to_owned(), play_params);
    let mut resource = json!({
        "id": id,
        "type": resource_type,
        "href": href,
        "attributes": Value::Object(attributes),
    });
    if let Some(tracks) = tracks
        && let Some(object) = resource.as_object_mut()
    {
        object.insert(
            "relationships".to_owned(),
            json!({"tracks": {"data": tracks}}),
        );
    }
    resource
}

fn genre_names(genre: Option<&str>) -> Value {
    match genre {
        Some(genre) => json!([genre]),
        None => json!([]),
    }
}

fn artwork_value(url: Option<&str>) -> Option<Value> {
    url.map(|url| json!({"url": url, "width": 0, "height": 0}))
}

fn insert_opt<S: AsRef<str>>(map: &mut Map<String, Value>, key: &str, value: Option<S>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), json!(value.as_ref()));
    }
}

// ----- JSON helpers -----------------------------------------------------------

fn parse_metadata(metadata: Option<&str>) -> Option<Value> {
    metadata.and_then(|m| serde_json::from_str(m).ok())
}

/// Ensures `document` is a JSON object, returning its map.
fn ensure_object(document: &mut Value) -> Option<&mut Map<String, Value>> {
    if !document.is_object() {
        *document = Value::Object(Map::new());
    }
    document.as_object_mut()
}

/// The `data` array of `document`, created (as empty) if absent.
fn data_array(document: &mut Value) -> Option<&mut Vec<Value>> {
    let object = ensure_object(document)?;
    object
        .entry("data")
        .or_insert_with(|| json!([]))
        .as_array_mut()
}

/// Replaces `document.data` with `items`.
fn set_data(document: &mut Value, items: Vec<Value>) {
    if let Some(object) = ensure_object(document) {
        object.insert("data".to_owned(), Value::Array(items));
    }
}

/// Prepends `items` to `document.data`, dropping any existing entry that shares
/// an injected item's id (custom wins). Returns whether anything was added.
/// Idempotent: a second call with the same items yields the same array.
fn prepend_unique(document: &mut Value, items: Vec<Value>) -> bool {
    if items.is_empty() {
        return false;
    }
    let injected: HashSet<String> = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let Some(data) = data_array(document) else {
        return false;
    };
    data.retain(|existing| {
        existing
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| !injected.contains(id))
    });
    data.splice(0..0, items);
    true
}

/// Adds `added` to `meta.total` when that field is present and numeric.
fn bump_total(document: &mut Value, added: usize) {
    let added = u64::try_from(added).unwrap_or(u64::MAX);
    if let Some(total) = document
        .pointer("/meta/total")
        .and_then(Value::as_u64)
        .map(|total| total.saturating_add(added))
        && let Some(slot) = document.pointer_mut("/meta/total")
    {
        *slot = json!(total);
    }
}

// ----- Interceptor ------------------------------------------------------------

/// Makes custom albums, playlists and songs appear in Cider's library (search,
/// listings, lookups and collection track pages). See the [module docs](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryInjectionInterceptor;

#[async_trait]
impl Interceptor for LibraryInjectionInterceptor {
    fn name(&self) -> &'static str {
        "library-injection"
    }

    async fn on_request(&self, exchange: &mut Exchange, request: Request<Body>) -> RequestOutcome {
        if *exchange.method() != Method::GET || !apple::is_api_host(exchange.host()) {
            return RequestOutcome::Continue(request);
        }
        match local_document(exchange.storage(), exchange.method(), exchange.uri()).await {
            Ok(Some(document)) => RequestOutcome::Respond(respond::json(
                StatusCode::OK,
                &document,
                exchange.request_headers(),
                "inject",
            )),
            Ok(None) => RequestOutcome::Continue(request),
            Err(error) => {
                tracing::warn!(%error, "custom-item lookup failed");
                RequestOutcome::Continue(request)
            }
        }
    }

    async fn on_response(
        &self,
        exchange: &mut Exchange,
        response: Response<Body>,
    ) -> Response<Body> {
        // The offline path merges into its own document; here we only augment a
        // real upstream response.
        if exchange.served_locally()
            || *exchange.method() != Method::GET
            || !apple::is_api_host(exchange.host())
            || !response.status().is_success()
        {
            return response;
        }
        if target(exchange.method(), exchange.uri()).is_none() {
            return response;
        }
        let is_json = content_type_essence(response.headers()).is_some_and(|t| t.ends_with("json"));
        if !is_json {
            return response;
        }
        let buffered = match buffer_decoded(response).await {
            Ok(Ok(buffered)) => buffered,
            Ok(Err(untouched)) => return untouched,
            Err(failed) => return failed,
        };
        let Ok(mut document) = serde_json::from_slice::<Value>(&buffered.body) else {
            return buffered.into_response();
        };
        let uri = exchange.uri().clone();
        match inject_into(exchange.storage(), &Method::GET, &uri, &mut document).await {
            Ok(true) => buffered.into_response_with(Bytes::from(document.to_string())),
            Ok(false) => buffered.into_response(),
            Err(error) => {
                tracing::warn!(%error, "library injection failed");
                buffered.into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CustomAudio, NewCustomCollection, NewCustomTrack};

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    async fn custom_song(storage: &Storage, title: &str, artist: &str) -> String {
        storage
            .create_custom_track(NewCustomTrack {
                title: title.to_owned(),
                artist: artist.to_owned(),
                album: Some("El Presidento".to_owned()),
                duration_ms: Some(165_493),
                audio: CustomAudio::LocalCopy("x.m4a".into()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap()
    }

    // ----- matching -----

    #[test]
    fn all_words_should_match_across_fields() {
        let track = Track::minimal("local-1", "Testing Song", "Example Band");
        assert!(matches(&track, "example TEST"));
    }

    #[test]
    fn missing_word_should_not_match() {
        let track = Track::minimal("local-1", "Testing Song", "Example Band");
        assert!(!matches(&track, "example nothing"));
    }

    #[test]
    fn types_without_songs_should_skip() {
        assert!(!wants_songs(Some("albums,artists"), SearchKind::Catalog));
    }

    // ----- search injection (shared by online + offline) -----

    #[tokio::test]
    async fn search_should_inject_for_catalog() {
        let storage = Storage::open_in_memory().unwrap();
        custom_song(&storage, "Testing Song", "Example Band").await;
        let mut doc = json!({"results": {"song": {"data": [{"id": "1"}]}}});
        assert!(
            inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/catalog/cz/search?term=example"),
                &mut doc,
            )
            .await
            .unwrap()
        );
        assert_eq!(
            doc.pointer("/results/song/data/0/attributes/name"),
            Some(&json!("Testing Song"))
        );
    }

    #[tokio::test]
    async fn search_offline_base_document_should_gain_meta_order() {
        let storage = Storage::open_in_memory().unwrap();
        custom_song(&storage, "Testing Song", "Example Band").await;
        // Offline builds an empty base document; the same injector fills it.
        let mut doc = json!({"results": {}});
        assert!(
            inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/catalog/cz/search?term=example"),
                &mut doc,
            )
            .await
            .unwrap()
        );
        assert_eq!(doc.pointer("/meta/results/order"), Some(&json!(["song"])));
    }

    // ----- library listings -----

    #[tokio::test]
    async fn library_songs_listing_should_prepend_custom_songs() {
        let storage = Storage::open_in_memory().unwrap();
        custom_song(&storage, "Testing Song", "Example Band").await;
        let mut doc = json!({"data": [{"id": "i.real"}], "meta": {"total": 1}});
        assert!(
            inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/me/library/songs?limit=100"),
                &mut doc,
            )
            .await
            .unwrap()
        );
        assert_eq!(doc.pointer("/data/0/type"), Some(&json!("library-songs")));
        assert_eq!(doc.pointer("/meta/total"), Some(&json!(2)));
    }

    #[tokio::test]
    async fn library_songs_listing_should_skip_later_pages() {
        let storage = Storage::open_in_memory().unwrap();
        custom_song(&storage, "Testing Song", "Example Band").await;
        let mut doc = json!({"data": [{"id": "i.real"}]});
        assert!(
            !inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/me/library/songs?offset=100"),
                &mut doc,
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn library_albums_listing_should_prepend_custom_album() {
        let storage = Storage::open_in_memory().unwrap();
        let album = storage
            .create_custom_collection(NewCustomCollection {
                kind: CollectionKind::Album,
                name: "Mixtape".to_owned(),
                artist: Some("Me".to_owned()),
                artwork_url: None,
                description: None,
                genre: None,
                release_date: None,
                metadata: None,
            })
            .await
            .unwrap();
        let mut doc = json!({"data": []});
        assert!(
            inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/me/library/albums"),
                &mut doc,
            )
            .await
            .unwrap()
        );
        assert_eq!(doc.pointer("/data/0/id"), Some(&json!(album)));
        assert_eq!(doc.pointer("/data/0/type"), Some(&json!("library-albums")));
    }

    // ----- fully-custom lookups answered locally -----

    #[tokio::test]
    async fn custom_song_lookup_should_build_local_document() {
        let storage = Storage::open_in_memory().unwrap();
        let id = custom_song(&storage, "Testing Song", "Example Band").await;
        let doc = local_document(
            &storage,
            &Method::GET,
            &uri(&format!(
                "https://amp-api.music.apple.com/v1/me/library/songs/{id}"
            )),
        )
        .await
        .unwrap()
        .expect("custom lookup should be local");
        assert_eq!(doc.pointer("/data/0/id"), Some(&json!(id)));
    }

    #[tokio::test]
    async fn mixed_lookup_should_not_be_local() {
        let storage = Storage::open_in_memory().unwrap();
        let id = custom_song(&storage, "Testing Song", "Example Band").await;
        let none = local_document(
            &storage,
            &Method::GET,
            &uri(&format!(
                "https://amp-api.music.apple.com/v1/catalog/us/songs?ids={id},123"
            )),
        )
        .await
        .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn custom_collection_tracks_should_be_ordered_members() {
        let storage = Storage::open_in_memory().unwrap();
        let album = storage
            .create_custom_collection(NewCustomCollection {
                kind: CollectionKind::Album,
                name: "Mixtape".to_owned(),
                artist: None,
                artwork_url: None,
                description: None,
                genre: None,
                release_date: None,
                metadata: None,
            })
            .await
            .unwrap();
        let a = custom_song(&storage, "A", "Me").await;
        let b = custom_song(&storage, "B", "Me").await;
        storage
            .place_track_in_collection(&album, &b, usize::MAX)
            .await
            .unwrap();
        storage
            .place_track_in_collection(&album, &a, usize::MAX)
            .await
            .unwrap();
        let doc = local_document(
            &storage,
            &Method::GET,
            &uri(&format!(
                "https://amp-api.music.apple.com/v1/me/library/albums/{album}/tracks"
            )),
        )
        .await
        .unwrap()
        .expect("local collection tracks");
        // b was placed first, so it is track 1.
        assert_eq!(doc.pointer("/data/0/id"), Some(&json!(b)));
        assert_eq!(
            doc.pointer("/data/0/attributes/trackNumber"),
            Some(&json!(1))
        );
        assert_eq!(doc.pointer("/data/1/id"), Some(&json!(a)));
        assert_eq!(
            doc.pointer("/data/1/attributes/trackNumber"),
            Some(&json!(2))
        );
    }

    // ----- real-album numbering -----

    #[tokio::test]
    async fn real_album_detail_should_inject_and_renumber() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .create_custom_track(NewCustomTrack {
                title: "Bonus".to_owned(),
                artist: "A".to_owned(),
                real_album_id: Some("12345".to_owned()),
                track_number: Some(2),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        // A real album with two real tracks at positions 1 and 2.
        let mut doc = json!({"data": [{
            "id": "12345",
            "type": "albums",
            "attributes": {"name": "Album", "trackCount": 2},
            "relationships": {"tracks": {"data": [
                {"id": "100", "type": "songs", "attributes": {"name": "One", "trackNumber": 1}},
                {"id": "101", "type": "songs", "attributes": {"name": "Two", "trackNumber": 2}}
            ]}}
        }]});
        assert!(
            inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/catalog/us/albums/12345"),
                &mut doc,
            )
            .await
            .unwrap()
        );
        // Bonus takes slot 2; the list is renumbered 1..=3 and trackCount updated.
        assert_eq!(
            doc.pointer("/data/0/relationships/tracks/data/1/attributes/name"),
            Some(&json!("Bonus"))
        );
        assert_eq!(
            doc.pointer("/data/0/relationships/tracks/data/2/attributes/trackNumber"),
            Some(&json!(3))
        );
        assert_eq!(
            doc.pointer("/data/0/attributes/trackCount"),
            Some(&json!(3))
        );
    }

    #[tokio::test]
    async fn real_album_tracks_page_should_inject_and_renumber() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .create_custom_track(NewCustomTrack {
                title: "Bonus".to_owned(),
                artist: "A".to_owned(),
                real_album_id: Some("12345".to_owned()),
                track_number: Some(1),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let mut doc = json!({"data": [
            {"id": "100", "type": "songs", "attributes": {"name": "One", "trackNumber": 1}}
        ]});
        assert!(
            inject_into(
                &storage,
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/catalog/us/albums/12345/tracks"),
                &mut doc,
            )
            .await
            .unwrap()
        );
        assert_eq!(
            doc.pointer("/data/0/attributes/name"),
            Some(&json!("Bonus"))
        );
        assert_eq!(
            doc.pointer("/data/1/attributes/trackNumber"),
            Some(&json!(2))
        );
    }

    // ----- dedupe / idempotency -----

    #[tokio::test]
    async fn listing_injection_should_be_idempotent() {
        let storage = Storage::open_in_memory().unwrap();
        custom_song(&storage, "Testing Song", "Example Band").await;
        let url = "https://amp-api.music.apple.com/v1/me/library/songs";
        let mut doc = json!({"data": [{"id": "i.real"}]});
        inject_into(&storage, &Method::GET, &uri(url), &mut doc)
            .await
            .unwrap();
        inject_into(&storage, &Method::GET, &uri(url), &mut doc)
            .await
            .unwrap();
        let data = doc.pointer("/data").and_then(Value::as_array).unwrap();
        // One custom + one real, not two customs.
        assert_eq!(data.len(), 2);
    }
}
