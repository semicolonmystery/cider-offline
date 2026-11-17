//! Extraction of library metadata (songs, albums, playlists and their track
//! lists) from Apple Music API JSON documents.

use hudsucker::hyper::Uri;
use serde_json::Value;

use crate::apple;
use crate::storage::{CatalogUpdate, CollectionKind, Playlist, TrackListUpdate, TrackMetadata};

/// Resource types treated as songs.
const SONG_TYPES: &[&str] = &["songs", "library-songs"];

/// Resource types a library playlist can list as entries: songs and music
/// videos (both playable; both count toward Apple's `meta.total`).
const PLAYABLE_TYPES: &[&str] = &[
    "songs",
    "library-songs",
    "music-videos",
    "library-music-videos",
];

/// Walks a JSON:API document and collects every song / album / playlist
/// resource with attributes, plus track lists from `relationships.tracks`
/// and from `…/{playlists|albums}/{id}/tracks` pages.
pub(crate) fn extract(document: &Value, uri: &Uri) -> CatalogUpdate {
    let mut update = CatalogUpdate::default();
    visit(document, &mut update);

    // Provenance: tag `in_library` on only the songs this response proves are
    // saved library members — the top-level `data[]` of a membership endpoint,
    // never a song nested inside a playlist's track relationship.
    if let Some(membership) = apple::library_membership(uri.path()) {
        tag_library_members(document, membership, &mut update);
    }

    if let Some(collection_id) = apple::collection_tracks_target(uri.path()) {
        let track_ids = song_ids(document.get("data"));
        if !track_ids.is_empty() {
            let offset = apple::query_param(uri, "offset")
                .and_then(|o| o.parse().ok())
                .unwrap_or(0);
            update.track_lists.push(TrackListUpdate {
                playlist_id: collection_id,
                track_ids,
                offset,
                complete: document.get("next").is_none(),
            });
        }
    }
    update
}

fn visit(value: &Value, update: &mut CatalogUpdate) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| visit(item, update)),
        Value::Object(map) => {
            resource(value, update);
            map.values().for_each(|child| visit(child, update));
        }
        _ => {}
    }
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn u32_at(value: &Value, pointer: &str) -> Option<u32> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

fn bool_at(value: &Value, pointer: &str) -> Option<bool> {
    value.pointer(pointer).and_then(Value::as_bool)
}

/// First entry of a `genreNames`-style string array.
fn first_genre(attributes: &Value) -> Option<String> {
    attributes
        .pointer("/genreNames/0")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Editorial notes / description, preferring the standard then the short form.
fn description_of(attributes: &Value) -> Option<String> {
    [
        "/description/standard",
        "/editorialNotes/standard",
        "/editorialNotes/short",
    ]
    .into_iter()
    .find_map(|pointer| str_at(attributes, pointer))
    .map(str::to_owned)
}

fn resource(value: &Value, update: &mut CatalogUpdate) {
    if let Some(track) = song_from_resource(value) {
        update.tracks.push(track);
        return;
    }
    let Some(collection) = collection_from_resource(value) else {
        return;
    };
    if let Some(tracks) = value.pointer("/relationships/tracks") {
        let track_ids = song_ids(tracks.get("data"));
        if !track_ids.is_empty() {
            update.track_lists.push(TrackListUpdate {
                playlist_id: collection.id.clone(),
                track_ids,
                offset: 0,
                complete: tracks.get("next").is_none(),
            });
        }
    }
    update.collections.push(collection);
}

/// Parses one song resource (`songs` / `library-songs`) with attributes.
///
/// A library song fetched with `include=catalog` carries its catalog twin at
/// `relationships.catalog.data[0]`; attributes the library resource lacks
/// (album artist, composer, ISRC, URL, time-synced lyrics flag, …) are filled
/// from that twin. The stored metadata is the resource verbatim.
pub(crate) fn song_from_resource(value: &Value) -> Option<TrackMetadata> {
    track_from_resource(value, SONG_TYPES)
}

/// Like [`song_from_resource`], but also accepts music videos — any entry a
/// library playlist / album track list can hold.
pub(crate) fn playable_from_resource(value: &Value) -> Option<TrackMetadata> {
    track_from_resource(value, PLAYABLE_TYPES)
}

fn track_from_resource(value: &Value, types: &[&str]) -> Option<TrackMetadata> {
    let id = str_at(value, "/id")?;
    let kind = str_at(value, "/type")?;
    if !types.contains(&kind) {
        return None;
    }
    let attributes = value.get("attributes").filter(|a| a.is_object())?;
    let name = str_at(attributes, "/name")?;
    let catalog = value
        .pointer("/relationships/catalog/data/0/attributes")
        .filter(|a| a.is_object());
    // A field from the resource itself, else from its catalog twin.
    let text = |pointer: &str| {
        str_at(attributes, pointer)
            .or_else(|| catalog.and_then(|c| str_at(c, pointer)))
            .map(str::to_owned)
    };
    let number = |pointer: &str| {
        u32_at(attributes, pointer).or_else(|| catalog.and_then(|c| u32_at(c, pointer)))
    };
    let flag = |pointer: &str| {
        bool_at(attributes, pointer).or_else(|| catalog.and_then(|c| bool_at(c, pointer)))
    };
    let catalog_id = str_at(attributes, "/playParams/catalogId")
        .or_else(|| str_at(value, "/relationships/catalog/data/0/id"))
        .filter(|c| *c != id)
        .map(str::to_owned);
    Some(TrackMetadata {
        id: id.to_owned(),
        catalog_id,
        title: name.to_owned(),
        artist: str_at(attributes, "/artistName")
            .unwrap_or_default()
            .to_owned(),
        album: text("/albumName"),
        duration_ms: number("/durationInMillis"),
        artwork_url: text("/artwork/url"),
        metadata: Some(value.to_string()),
        album_artist: text("/albumArtistName"),
        composer: text("/composerName"),
        genre: first_genre(attributes).or_else(|| catalog.and_then(first_genre)),
        track_number: number("/trackNumber"),
        disc_number: number("/discNumber"),
        release_date: text("/releaseDate"),
        isrc: text("/isrc"),
        url: text("/url"),
        has_lyrics: flag("/hasLyrics"),
        has_time_synced_lyrics: flag("/hasTimeSyncedLyrics"),
        content_rating: text("/contentRating"),
        // Set later by `tag_library_members` for exactly the top-level saved
        // members of a membership endpoint; every resource starts untagged.
        in_library: false,
    })
}

/// Parses one album / playlist resource (catalog or library) with attributes.
pub(crate) fn collection_from_resource(value: &Value) -> Option<Playlist> {
    let id = str_at(value, "/id")?;
    let kind = str_at(value, "/type")?;
    let attributes = value.get("attributes").filter(|a| a.is_object())?;
    let name = str_at(attributes, "/name")?;
    let (collection_kind, artist) = match kind {
        "albums" | "library-albums" => (CollectionKind::Album, "/artistName"),
        "playlists" | "library-playlists" => (CollectionKind::Playlist, "/curatorName"),
        _ => return None,
    };
    Some(Playlist {
        id: id.to_owned(),
        kind: collection_kind,
        name: name.to_owned(),
        artist: str_at(attributes, artist).map(str::to_owned),
        artwork_url: str_at(attributes, "/artwork/url").map(str::to_owned),
        metadata: Some(value.to_string()),
        is_custom: false,
        description: description_of(attributes),
        genre: first_genre(attributes),
        release_date: str_at(attributes, "/releaseDate").map(str::to_owned),
    })
}

/// Sets `in_library = true` on every collected track whose id is a saved member
/// of this membership response. Members are the song ids found at the location
/// the endpoint keeps them (top-level `data[]`, or a saved album's
/// `relationships.tracks.data[]`), so nested playlist songs are never tagged.
fn tag_library_members(
    document: &Value,
    membership: apple::LibraryMembership,
    update: &mut CatalogUpdate,
) {
    let data = match membership {
        apple::LibraryMembership::TopLevelData => document.get("data"),
        apple::LibraryMembership::AlbumTracks => {
            document.pointer("/data/0/relationships/tracks/data")
        }
    };
    let members: std::collections::HashSet<String> = song_ids(data).into_iter().collect();
    if members.is_empty() {
        return;
    }
    for track in &mut update.tracks {
        if members.contains(&track.id) {
            track.in_library = true;
        }
    }
}

fn song_ids(data: Option<&Value>) -> Vec<String> {
    data.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| str_at(item, "/type").is_some_and(|t| SONG_TYPES.contains(&t)))
                .filter_map(|item| str_at(item, "/id").map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn library_song() -> Value {
        json!({"data":[{"id":"a.1594405597","type":"library-songs","attributes":{
            "albumName":"Album","artistName":"Example Artist","durationInMillis":189_296,
            "name":"Example Song","artwork":{"url":"https://is1-ssl.mzstatic.com/x/{w}x{h}bb.jpg"},
            "playParams":{"catalogId":"1594405597","id":"a.1594405597","isLibrary":true,"kind":"song"}}}]})
    }

    #[test]
    fn library_song_should_extract_catalog_id() {
        let update = extract(
            &library_song(),
            &uri("https://amp-api.music.apple.com/v1/me/library/songs"),
        );
        assert_eq!(update.tracks[0].catalog_id.as_deref(), Some("1594405597"));
    }

    #[test]
    fn library_song_should_extract_duration() {
        let update = extract(
            &library_song(),
            &uri("https://amp-api.music.apple.com/v1/me/library/songs"),
        );
        assert_eq!(update.tracks[0].duration_ms, Some(189_296));
    }

    #[test]
    fn resource_without_attributes_should_be_skipped() {
        let doc = json!({"data":[{"id":"1","type":"songs","href":"/v1/catalog/us/songs/1"}]});
        assert!(extract(&doc, &uri("https://h/v1/catalog/us/songs")).is_empty());
    }

    #[test]
    fn playlist_relationship_should_yield_track_list() {
        let doc = json!({"data":[{"id":"p.1","type":"library-playlists",
            "attributes":{"name":"Mix","curatorName":"Me"},
            "relationships":{"tracks":{"data":[
                {"id":"i.1","type":"library-songs","attributes":{"name":"A","artistName":"X"}},
                {"id":"i.2","type":"library-songs"}]}}}]});
        let update = extract(&doc, &uri("https://h/v1/me/library/playlists/p.1"));
        assert_eq!(
            update.track_lists,
            [TrackListUpdate {
                playlist_id: "p.1".to_owned(),
                track_ids: vec!["i.1".to_owned(), "i.2".to_owned()],
                offset: 0,
                complete: true,
            }]
        );
    }

    #[test]
    fn nested_song_should_be_collected() {
        let doc = json!({"data":[{"id":"p.1","type":"library-playlists",
            "attributes":{"name":"Mix"},
            "relationships":{"tracks":{"data":[
                {"id":"i.1","type":"library-songs","attributes":{"name":"A","artistName":"X"}}]}}}]});
        let update = extract(&doc, &uri("https://h/v1/me/library/playlists/p.1"));
        assert_eq!(update.tracks.len(), 1);
    }

    #[test]
    fn tracks_page_should_use_offset_and_next() {
        let doc = json!({"next":"/v1/me/library/playlists/p.1/tracks?offset=200",
            "data":[{"id":"i.9","type":"library-songs"}]});
        let update = extract(
            &doc,
            &uri("https://h/v1/me/library/playlists/p.1/tracks?offset=100"),
        );
        let list = &update.track_lists[0];
        assert!(list.offset == 100 && !list.complete);
    }

    #[test]
    fn library_songs_list_should_tag_in_library() {
        let update = extract(
            &library_song(),
            &uri("https://amp-api.music.apple.com/v1/me/library/songs?offset=100&l=en"),
        );
        assert!(update.tracks[0].in_library);
    }

    #[test]
    fn playlist_tracks_should_not_tag_in_library() {
        // A song reached only inside a playlist's track relationship is NOT a
        // saved library member, even though it is still collected.
        let doc = json!({"data":[{"id":"p.1","type":"library-playlists",
            "attributes":{"name":"Mix","curatorName":"Me"},
            "relationships":{"tracks":{"data":[
                {"id":"i.1","type":"library-songs","attributes":{"name":"A","artistName":"X"}}]}}}]});
        let update = extract(
            &doc,
            &uri("https://amp-api.music.apple.com/v1/me/library/playlists/p.1/tracks"),
        );
        assert_eq!(update.tracks.len(), 1);
        assert!(!update.tracks[0].in_library);
    }

    #[test]
    fn library_album_form_should_tag_its_relationship_tracks() {
        let doc = json!({"data":[{"id":"l.al","type":"library-albums",
            "attributes":{"name":"Al","artistName":"Ar"},
            "relationships":{"tracks":{"data":[
                {"id":"i.9","type":"library-songs","attributes":{"name":"S","artistName":"Ar"}}]}}}]});
        let update = extract(
            &doc,
            &uri("https://amp-api.music.apple.com/v1/me/library/albums/l.al"),
        );
        let song = update.tracks.iter().find(|t| t.id == "i.9").unwrap();
        assert!(song.in_library);
    }

    #[test]
    fn catalog_song_should_not_tag_in_library() {
        let doc = json!({"data":[{"id":"1","type":"songs",
            "attributes":{"name":"S","artistName":"Ar"}}]});
        let update = extract(&doc, &uri("https://h/v1/catalog/us/songs/1"));
        assert!(!update.tracks[0].in_library);
    }

    #[test]
    fn album_should_use_artist_name() {
        let doc = json!({"data":[{"id":"1","type":"albums",
            "attributes":{"name":"Al","artistName":"Ar"}}]});
        let update = extract(&doc, &uri("https://h/v1/catalog/us/albums/1"));
        assert_eq!(update.collections[0].artist.as_deref(), Some("Ar"));
    }
}
