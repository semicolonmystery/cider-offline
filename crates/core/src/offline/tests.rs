use hudsucker::hyper::HeaderMap;
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;

use super::*;
use crate::cache::STOREFRONT_SETTING;
use crate::proxy::body::read_body;
use crate::storage::{
    CatalogUpdate, CollectionKind, LibrarySnapshot, Playlist, SyncedCollection, TrackMetadata,
};

fn exchange(storage: &Storage, url: &str) -> Exchange {
    let request = Request::builder().uri(url).body(Body::empty()).unwrap();
    Exchange::new(1, &request, false, storage.clone(), TaskTracker::new())
}

async fn body_json(response: Response<Body>) -> Value {
    serde_json::from_slice(&read_body(response.into_body()).await.unwrap()).unwrap()
}

fn responder(dir: &tempfile::TempDir, storage: &Storage) -> OfflineResponder {
    responder_with(dir, storage, SongsPage::Apple)
}

fn responder_with(dir: &tempfile::TempDir, storage: &Storage, page: SongsPage) -> OfflineResponder {
    OfflineResponder::new(
        ArtworkStore::new(dir.path().join("artwork"), storage.clone()),
        Arc::new(AtomicU8::new(page.as_u8())),
    )
}

async fn get(dir: &tempfile::TempDir, storage: &Storage, url: &str) -> Value {
    body_json(
        responder(dir, storage)
            .answer(&exchange(storage, url))
            .await,
    )
    .await
}

/// A library song `a.<n>` (catalog twin `<n>`) on catalog album `900`.
fn song(n: u32, title: &str) -> TrackMetadata {
    let resource = json!({
        "id": format!("a.{n}"),
        "type": "library-songs",
        "href": format!("/v1/me/library/songs/a.{n}"),
        "attributes": {
            "name": title,
            "artistName": "Artist",
            "albumName": "Album",
            "trackNumber": n,
            "artwork": {"url": "https://is1-ssl.mzstatic.com/image/thumb/x/cover.jpg/{w}x{h}bb.jpg"},
            "playParams": {"id": format!("a.{n}"), "catalogId": n.to_string(), "isLibrary": true, "kind": "song"},
        },
        "relationships": {"catalog": {"data": [{
            "id": n.to_string(),
            "type": "songs",
            "href": format!("/v1/catalog/cz/songs/{n}"),
            "attributes": {
                "name": title,
                "artistName": "Artist",
                "albumName": "Album",
                "url": format!("https://music.apple.com/cz/album/album/900?i={n}"),
                "playParams": {"id": n.to_string(), "kind": "song"},
            },
        }]}},
    });
    TrackMetadata {
        id: format!("a.{n}"),
        catalog_id: Some(n.to_string()),
        title: title.to_owned(),
        artist: "Artist".to_owned(),
        album: Some("Album".to_owned()),
        metadata: Some(resource.to_string()),
        ..TrackMetadata::default()
    }
}

fn playlist(id: &str, name: &str, members: &[u32]) -> SyncedCollection {
    let resource = json!({
        "id": id,
        "type": "library-playlists",
        "href": format!("/v1/me/library/playlists/{id}"),
        "attributes": {
            "name": name,
            "canEdit": true,
            "dateAdded": "2025-01-01T00:00:00Z",
            "playParams": {"id": id, "globalId": format!("pl.u-{id}"), "isLibrary": true, "kind": "playlist"},
        },
    });
    SyncedCollection {
        collection: Playlist {
            metadata: Some(resource.to_string()),
            ..Playlist::minimal(id, CollectionKind::Playlist, name)
        },
        track_ids: members.iter().map(|n| format!("a.{n}")).collect(),
        reported_total: u32::try_from(members.len()).ok(),
    }
}

/// A store with playlist `p.big` (1..=150), playlist `p.small` (3, 2) and
/// tracks 1..=150; `downloaded` tracks get an audio file in `dir`.
async fn library(dir: &tempfile::TempDir, downloaded: &[u32]) -> Storage {
    let storage = Storage::open_in_memory().unwrap();
    let mut tracks: Vec<TrackMetadata> = (1..=150).map(|n| song(n, &format!("Song {n}"))).collect();
    tracks[0] = song(1, "Žluťoučký kůň");
    let big: Vec<u32> = (1..=150).collect();
    storage
        .replace_library(LibrarySnapshot {
            collections: vec![
                playlist("p.big", "Big", &big),
                playlist("p.small", "Crème Brûlée", &[3, 2]),
            ],
            tracks,
            songs: Vec::new(),
        })
        .await
        .unwrap();
    for n in downloaded {
        let file = dir.path().join(format!("{n}.m4a"));
        std::fs::write(&file, b"audio").unwrap();
        storage
            .set_audio_by_any_id(
                &n.to_string(),
                crate::storage::AudioVariant::Stereo,
                Some(file),
            )
            .await
            .unwrap();
    }
    storage
}

#[tokio::test]
async fn playlist_folder_should_list_synced_playlists() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlist-folders/p.playlistsroot/children?offset=0&limit=100&l=en-GB",
    )
    .await;
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        (ids, body.pointer("/meta/total")),
        (vec!["p.big", "p.small"], Some(&json!(2)))
    );
}

#[tokio::test]
async fn playlist_tracks_should_paginate_with_next_link() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.big/tracks?offset=0&l=en-GB&limit=100&include=albums",
    )
    .await;
    assert_eq!(
        (
            body["data"].as_array().unwrap().len(),
            body["next"].as_str(),
            body.pointer("/meta/total")
        ),
        (
            100,
            Some("/v1/me/library/playlists/p.big/tracks?l=en-GB&offset=100"),
            Some(&json!(150))
        )
    );
}

#[tokio::test]
async fn last_playlist_page_should_have_no_next_link() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.big/tracks?offset=100&limit=100",
    )
    .await;
    assert_eq!(
        (
            body["data"].as_array().unwrap().len(),
            body.get("next"),
            body["data"][0]["id"].as_str()
        ),
        (50, None, Some("a.101"))
    );
}

#[tokio::test]
async fn playlist_tracks_should_keep_synced_order() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.small/tracks",
    )
    .await;
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a.3", "a.2"]);
}

#[tokio::test]
async fn playlist_detail_should_embed_tracks_with_catalog_when_included() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.big?include=catalog,tracks&limit%5Btracks%5D=100",
    )
    .await;
    let tracks = &body["data"][0]["relationships"]["tracks"];
    assert_eq!(
        (
            tracks["data"].as_array().unwrap().len(),
            tracks.pointer("/meta/total"),
            tracks["data"][0].pointer("/relationships/catalog/data/0/type")
        ),
        (100, Some(&json!(150)), Some(&json!("songs")))
    );
}

#[tokio::test]
async fn playlist_detail_without_include_should_omit_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.small?fields=name",
    )
    .await;
    assert_eq!(
        body["data"][0],
        json!({
            "id": "p.small",
            "type": "library-playlists",
            "href": "/v1/me/library/playlists/p.small",
            "attributes": {"name": "Crème Brûlée"},
        })
    );
}

#[tokio::test]
async fn downloaded_song_should_keep_play_params() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[2]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.small/tracks",
    )
    .await;
    assert_eq!(
        (
            body.pointer("/data/0/attributes/playParams").is_some(),
            body.pointer("/data/1/attributes/playParams").is_some()
        ),
        (false, true)
    );
}

#[tokio::test]
async fn catalog_song_lookup_should_accept_library_id() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/us/songs/a.7?fields=audioLocale,name",
    )
    .await;
    assert_eq!(
        (
            body.pointer("/data/0/id"),
            body.pointer("/data/0/attributes")
        ),
        (Some(&json!("7")), Some(&json!({"name": "Song 7"})))
    );
}

#[tokio::test]
async fn unknown_song_should_be_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let response = responder(&dir, &storage)
        .answer(&exchange(
            &storage,
            "https://amp-api.music.apple.com/v1/catalog/us/songs/999999",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resource_map_lookup_should_normalize_songs() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz?ids%5Bsongs%5D=4,5&fields%5Bsongs%5D=inLibrary,inFavorites&relate=library&format%5Bresources%5D=map",
    )
    .await;
    assert_eq!(
        (
            body["data"].as_array().unwrap().len(),
            body.pointer("/resources/songs/4/attributes"),
            body.pointer("/resources/songs/5/relationships/library/data")
        ),
        (
            2,
            Some(&json!({"inLibrary": false, "inFavorites": false})),
            Some(&json!([]))
        )
    );
}

#[tokio::test]
async fn catalog_playlist_should_resolve_global_id() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz/playlists/pl.u-p.small?include%5Bplaylists%5D=tracks",
    )
    .await;
    assert_eq!(
        body.pointer("/data/0/relationships/tracks/data/0/id"),
        Some(&json!("3"))
    );
}

#[tokio::test]
async fn catalog_album_should_collect_local_tracks_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz/albums/900",
    )
    .await;
    assert_eq!(
        (
            body.pointer("/data/0/attributes/name"),
            body.pointer("/data/0/relationships/tracks/data/1/id"),
            body.pointer("/data/0/relationships/tracks/meta/total")
        ),
        (Some(&json!("Album")), Some(&json!("2")), Some(&json!(150)))
    );
}

#[tokio::test]
async fn search_should_be_accent_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz/search?term=zlutoucky&types=songs&with=serverBubbles",
    )
    .await;
    assert_eq!(
        body.pointer("/results/song/data/0/attributes/name"),
        Some(&json!("Žluťoučký kůň"))
    );
}

#[tokio::test]
async fn search_should_rank_downloaded_first() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[120]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz/search?term=song&types=songs&limit=5",
    )
    .await;
    assert_eq!(
        body.pointer("/results/songs/data/0/id"),
        Some(&json!("120"))
    );
}

#[tokio::test]
async fn library_search_should_find_playlists() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/search?term=creme&types=library-playlists",
    )
    .await;
    assert_eq!(
        body.pointer("/results/library-playlists/data/0/id"),
        Some(&json!("p.small"))
    );
}

#[tokio::test]
async fn suggestions_should_offer_term_and_top_results() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz/search/suggestions?term=song%2012&kinds=terms,topResults&types=songs",
    )
    .await;
    let kinds: Vec<&str> = body
        .pointer("/results/suggestions")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first().copied(), Some("terms"));
    assert!(kinds.contains(&"topResults"));
}

#[tokio::test]
async fn cached_entry_should_be_served_for_unowned_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let url = "https://amp-api.music.apple.com/v1/editorial/cz/groupings?name=x";
    let ex = exchange(&storage, url);
    storage
        .put_cache_entry(CacheEntry {
            key: ex.cache_key().unwrap(),
            method: "GET".to_owned(),
            url: url.to_owned(),
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: br#"{"data":[{"id":"g"}]}"#.to_vec(),
            stored_at: 0,
        })
        .await
        .unwrap();
    let response = responder(&dir, &storage).answer(&ex).await;
    assert_eq!(response.headers()[LOCAL_HEADER], "cache");
    assert_eq!(body_json(response).await, json!({"data":[{"id":"g"}]}));
}

#[tokio::test]
async fn storefront_should_be_derived_from_synced_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let storage = library(&dir, &[]).await;
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/storefront",
    )
    .await;
    assert_eq!(body.pointer("/data/0/id"), Some(&json!("cz")));
}

#[tokio::test]
async fn storefront_should_fall_back_to_minimal_document() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/storefront",
    )
    .await;
    assert_eq!(body.pointer("/data/0/id"), Some(&json!("us")));
}

#[tokio::test]
async fn persisted_storefront_should_be_served() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let document = api::storefront_document("cz", "en-GB");
    storage
        .set_setting(STOREFRONT_SETTING, document.to_string())
        .await
        .unwrap();
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/storefront?l=cs",
    )
    .await;
    assert_eq!(body, document);
}

#[tokio::test]
async fn unknown_endpoint_should_get_empty_data() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/social-profile",
    )
    .await;
    assert_eq!(body, json!({"data": []}));
}

#[tokio::test]
async fn song_lookup_should_use_stored_catalog_row() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    storage
        .apply_catalog_update(CatalogUpdate {
            tracks: vec![TrackMetadata {
                id: "5".to_owned(),
                title: "T".to_owned(),
                artist: "A".to_owned(),
                metadata: Some(r#"{"id":"5","type":"songs"}"#.to_owned()),
                ..TrackMetadata::default()
            }],
            ..CatalogUpdate::default()
        })
        .await
        .unwrap();
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/catalog/cz/songs/5",
    )
    .await;
    assert_eq!(body.pointer("/data/0/id"), Some(&json!("5")));
}

#[tokio::test]
async fn missing_artwork_should_be_404() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let ex = exchange(
        &storage,
        "https://is1-ssl.mzstatic.com/image/a.jpg/1x1bb.jpg",
    );
    let response = responder(&dir, &storage).answer(&ex).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

async fn store_artwork(store: &ArtworkStore, url: &str, body: &'static [u8]) {
    store
        .store(
            &url.parse().unwrap(),
            "image/jpeg",
            Bytes::from_static(body),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn artwork_should_serve_smallest_variant_covering_requested_size() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let responder = responder(&dir, &storage);
    let base = "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/a/cover.jpg";
    store_artwork(
        &responder.artwork,
        &format!("{base}/120x120bb.jpg"),
        b"small",
    )
    .await;
    store_artwork(
        &responder.artwork,
        &format!("{base}/600x600bb.jpg"),
        b"large",
    )
    .await;
    let response = responder
        .answer(&exchange(&storage, &format!("{base}/300x300bb.webp")))
        .await;
    assert_eq!(
        (
            response.headers()[CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_owned(),
            read_body(response.into_body()).await.unwrap()
        ),
        ("image/jpeg".to_owned(), Bytes::from_static(b"large"))
    );
}

#[tokio::test]
async fn artwork_should_fall_back_to_largest_variant() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let responder = responder(&dir, &storage);
    let base = "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/a/cover.jpg";
    store_artwork(
        &responder.artwork,
        &format!("{base}/120x120bb.jpg"),
        b"small",
    )
    .await;
    let response = responder
        .answer(&exchange(&storage, &format!("{base}/1200x1200cc.jpg")))
        .await;
    assert_eq!(
        read_body(response.into_body()).await.unwrap(),
        Bytes::from_static(b"small")
    );
}

#[tokio::test]
async fn license_request_should_be_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense")
        .body(Body::empty())
        .unwrap();
    let ex = Exchange::new(1, &request, false, storage.clone(), TaskTracker::new());
    let response = responder(&dir, &storage).answer(&ex).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn cached_response_should_replace_cors_origin() {
    let entry = CacheEntry {
        key: String::new(),
        method: "GET".to_owned(),
        url: String::new(),
        status: 200,
        headers: vec![(
            "access-control-allow-origin".to_owned(),
            "https://old".to_owned(),
        )],
        body: Vec::new(),
        stored_at: 0,
    };
    let mut request_headers = HeaderMap::new();
    request_headers.insert("origin", HeaderValue::from_static("https://new"));
    let response = cached_response(entry, &request_headers);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://new"
    );
}

#[tokio::test]
async fn downloaded_song_without_play_params_should_become_playable() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let mut track = song(7, "Unstreamable");
    let mut resource: Value = serde_json::from_str(track.metadata.as_deref().unwrap()).unwrap();
    resource["attributes"]
        .as_object_mut()
        .unwrap()
        .remove("playParams");
    track.metadata = Some(resource.to_string());
    storage
        .replace_library(LibrarySnapshot {
            collections: vec![playlist("p.one", "One", &[7])],
            tracks: vec![track],
            songs: Vec::new(),
        })
        .await
        .unwrap();
    let file = dir.path().join("7.m4a");
    std::fs::write(&file, b"audio").unwrap();
    storage
        .set_audio_by_any_id("7", crate::storage::AudioVariant::Stereo, Some(file))
        .await
        .unwrap();
    let body = get(
        &dir,
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/playlists/p.one/tracks",
    )
    .await;
    assert_eq!(
        body.pointer("/data/0/attributes/playParams"),
        Some(&json!({"id": "a.7", "kind": "song", "isLibrary": true, "catalogId": "7"}))
    );
}

/// A store whose library Songs tab is `tab` (in that order) and whose other
/// saved library song is `i.9` (a library song only in a playlist).
async fn songs_library(tab: &[u32]) -> Storage {
    let storage = Storage::open_in_memory().unwrap();
    let mut tracks: Vec<TrackMetadata> = (1..=3).map(|n| song(n, &format!("Song {n}"))).collect();
    let mut only_in_playlist = song(9, "Playlist only");
    only_in_playlist.id = "i.9".to_owned();
    only_in_playlist.metadata = only_in_playlist.metadata.map(|m| {
        m.replace("\"a.9\"", "\"i.9\"")
            .replace("songs/a.9", "songs/i.9")
    });
    tracks.push(only_in_playlist);
    let mut in_playlist = playlist("p.one", "One", &[]);
    in_playlist.track_ids = vec!["i.9".to_owned()];
    storage
        .replace_library(LibrarySnapshot {
            collections: vec![in_playlist],
            tracks,
            songs: tab.iter().map(|n| format!("a.{n}")).collect(),
        })
        .await
        .unwrap();
    storage
}

async fn song_ids(responder: &OfflineResponder, storage: &Storage, url: &str) -> Vec<String> {
    let body = body_json(responder.answer(&exchange(storage, url)).await).await;
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn apple_songs_page_should_list_synced_tab_in_date_added_order() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[3, 1]).await;
    let ids = song_ids(
        &responder(&dir, &storage),
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?offset=0&limit=100&sort=dateAdded",
    )
    .await;
    assert_eq!(ids, ["a.3", "a.1"]);
}

#[tokio::test]
async fn apple_songs_page_should_sort_by_name_without_sort() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[3, 1]).await;
    let ids = song_ids(
        &responder(&dir, &storage),
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?limit=100",
    )
    .await;
    assert_eq!(ids, ["a.1", "a.3"]);
}

#[tokio::test]
async fn all_songs_page_should_include_playlist_only_library_songs() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[3, 1]).await;
    let ids = song_ids(
        &responder_with(&dir, &storage, SongsPage::All),
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?limit=100",
    )
    .await;
    assert!(ids.contains(&"i.9".to_owned()));
}

#[tokio::test]
async fn apple_songs_page_should_still_resolve_id_lookups_outside_the_tab() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[1]).await;
    let ids = song_ids(
        &responder(&dir, &storage),
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?ids=i.9,a.2",
    )
    .await;
    assert_eq!(ids, ["i.9", "a.2"]);
}

#[tokio::test]
async fn songs_page_setting_should_apply_without_rebuilding_the_responder() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[1]).await;
    let page = Arc::new(AtomicU8::new(SongsPage::Apple.as_u8()));
    let responder = OfflineResponder::new(
        ArtworkStore::new(dir.path().join("artwork"), storage.clone()),
        Arc::clone(&page),
    );
    let url = "https://amp-api.music.apple.com/v1/me/library/songs?limit=100";
    let before = song_ids(&responder, &storage, url).await.len();
    page.store(SongsPage::All.as_u8(), Ordering::Relaxed);
    let after = song_ids(&responder, &storage, url).await.len();
    assert_eq!((before, after), (1, 2));
}

fn online_exchange(storage: &Storage, url: &str) -> Exchange {
    let request = Request::builder().uri(url).body(Body::empty()).unwrap();
    Exchange::new(1, &request, true, storage.clone(), TaskTracker::new())
}

#[tokio::test]
async fn online_all_songs_page_should_be_answered_locally() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[1]).await;
    let responder = responder_with(&dir, &storage, SongsPage::All);
    let exchange = online_exchange(
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?limit=100",
    );
    assert!(responder.answers_online(&exchange));
}

#[tokio::test]
async fn online_apple_songs_page_should_go_to_apple() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[1]).await;
    let exchange = online_exchange(
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?limit=100",
    );
    assert!(!responder(&dir, &storage).answers_online(&exchange));
}

#[tokio::test]
async fn online_id_lookup_should_go_to_apple_in_all_mode() {
    let dir = tempfile::tempdir().unwrap();
    let storage = songs_library(&[1]).await;
    let exchange = online_exchange(
        &storage,
        "https://amp-api.music.apple.com/v1/me/library/songs?ids=i.9",
    );
    assert!(!responder_with(&dir, &storage, SongsPage::All).answers_online(&exchange));
}
