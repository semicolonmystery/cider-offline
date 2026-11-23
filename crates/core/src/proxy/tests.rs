//! End-to-end tests of `ProxyHandle` on an ephemeral port.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hudsucker::hyper::{Request, Response};
use hudsucker::{Body, RequestOrResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::config::{ConnectivitySettings, ProxySettings};

/// Answers every request locally and counts calls.
#[derive(Default)]
struct LocalResponder {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Interceptor for LocalResponder {
    fn name(&self) -> &'static str {
        "test-responder"
    }

    async fn on_request(&self, _: &mut Exchange, _: Request<Body>) -> RequestOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = Response::builder()
            .header("content-type", "application/json")
            .body(Body::from("{\"local\":true}"))
            .unwrap();
        RequestOutcome::Respond(response)
    }
}

fn test_config(mode: Mode) -> Config {
    Config {
        mode,
        proxy: ProxySettings {
            port: 0,
            ..ProxySettings::default()
        },
        connectivity: ConnectivitySettings {
            enabled: false,
            ..ConnectivitySettings::default()
        },
        ..Config::default()
    }
}

async fn start(
    dir: &tempfile::TempDir,
    mode: Mode,
    responder: Option<LocalResponder>,
) -> ProxyHandle {
    let mut builder = ProxyHandle::builder(test_config(mode)).paths(AppPaths::under(dir.path()));
    if let Some(responder) = responder {
        builder = builder.interceptor(responder);
    }
    builder.start().await.unwrap()
}

fn client_via(proxy: SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{proxy}")).unwrap())
        .build()
        .unwrap()
}

/// Minimal HTTP/1.1 origin that answers one request with `ok`.
async fn spawn_origin() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    addr
}

fn apple_request() -> Request<Body> {
    Request::builder()
        .uri("https://amp-api.music.apple.com/v1/me/storefront")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn start_should_publish_initial_status() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let status = handle.status();
    assert!(status.running && status.mode == Mode::Active && status.listening.port() != 0);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_mode_should_notify_subscribers() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let mut rx = handle.subscribe();
    rx.borrow_and_update();
    handle.set_mode(Mode::Passthrough);
    rx.changed().await.unwrap();
    assert_eq!(rx.borrow().mode, Mode::Passthrough);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_mode_to_same_value_should_not_notify() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let mut rx = handle.subscribe();
    rx.borrow_and_update();
    handle.set_mode(Mode::Active);
    assert!(!rx.has_changed().unwrap());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_capture_should_update_status() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    handle.set_capture(true);
    assert!(handle.status().capture);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn passthrough_should_skip_interceptors_after_runtime_switch() {
    let dir = tempfile::tempdir().unwrap();
    let responder = LocalResponder::default();
    let calls = Arc::clone(&responder.calls);
    let handle = start(&dir, Mode::Active, Some(responder)).await;
    handle.set_mode(Mode::Passthrough);

    let mut handler = ProxyHttpHandler::new(Arc::clone(&handle.shared));
    let outcome = handler.process_request(apple_request()).await;

    assert!(matches!(outcome, RequestOrResponse::Request(_)) && calls.load(Ordering::SeqCst) == 0);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn active_should_route_apple_hosts_through_interceptors() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, Some(LocalResponder::default())).await;

    let mut handler = ProxyHttpHandler::new(Arc::clone(&handle.shared));
    let outcome = handler.process_request(apple_request()).await;

    assert!(matches!(outcome, RequestOrResponse::Response(_)));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn active_should_not_route_other_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, Some(LocalResponder::default())).await;

    let mut handler = ProxyHttpHandler::new(Arc::clone(&handle.shared));
    let request = Request::builder()
        .uri("https://example.com/")
        .body(Body::empty())
        .unwrap();

    assert!(matches!(
        handler.process_request(request).await,
        RequestOrResponse::Request(_)
    ));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn passthrough_should_forward_plain_http_and_count_request() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Passthrough, None).await;
    let origin = spawn_origin().await;

    let body = client_via(handle.local_addr())
        .get(format!("http://{origin}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(
        (body.as_str(), handle.status().counters.requests_seen),
        ("ok", 1)
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_answer_should_reach_client_and_count_as_served_locally() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, Some(LocalResponder::default())).await;

    // Plain-HTTP request to an Apple host: answered by the responder, so no
    // network access happens.
    let body = client_via(handle.local_addr())
        .get("http://amp-api.music.apple.com/v1/test")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(
        (body.as_str(), handle.status().counters.served_locally),
        ("{\"local\":true}", 1)
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn capture_should_write_json_file() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, Some(LocalResponder::default())).await;
    handle.set_capture(true);
    let captures = handle.paths().captures_dir();

    client_via(handle.local_addr())
        .get("http://amp-api.music.apple.com/v1/test")
        .header("authorization", "Bearer do-not-write-me")
        .send()
        .await
        .unwrap();
    handle.shutdown().await.unwrap(); // drains the background write

    let file = std::fs::read_dir(&captures)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(text.contains("\"local\\\":true") && !text.contains("do-not-write-me"));
}

#[tokio::test]
async fn active_should_capture_tokens_to_storage() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, Some(LocalResponder::default())).await;
    let storage = handle.storage().clone();

    client_via(handle.local_addr())
        .get("http://amp-api.music.apple.com/v1/test")
        .header("media-user-token", "mut-value")
        .send()
        .await
        .unwrap();
    handle.shutdown().await.unwrap(); // drains the background write

    assert_eq!(
        storage
            .get_auth_token(crate::storage::TokenKind::MediaUserToken)
            .await
            .unwrap()
            .as_deref(),
        Some("mut-value")
    );
}

#[tokio::test]
async fn bind_conflict_should_return_bind_error() {
    let dir = tempfile::tempdir().unwrap();
    let first = start(&dir, Mode::Active, None).await;
    let mut config = test_config(Mode::Active);
    config.proxy.port = first.local_addr().port();
    let other = tempfile::tempdir().unwrap();
    let err = ProxyHandle::builder(config)
        .paths(AppPaths::under(other.path()))
        .start()
        .await
        .unwrap_err();
    assert!(matches!(err, ProxyError::Bind { .. }));
    first.shutdown().await.unwrap();
}

fn set_online(handle: &ProxyHandle, online: bool) {
    handle.shared.status.send_modify(|s| s.online = online);
}

#[tokio::test]
async fn offline_storefront_should_be_synthesized_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    set_online(&handle, false);

    let body: serde_json::Value = client_via(handle.local_addr())
        .get("http://amp-api.music.apple.com/v1/me/storefront")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        (
            body.pointer("/data/0/type").and_then(|v| v.as_str()),
            handle.status().counters.served_locally
        ),
        (Some("storefronts"), 1)
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_media_should_be_served_with_range() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let file = dir.path().join("song.mp3");
    std::fs::write(&file, b"0123456789").unwrap();
    handle
        .storage()
        .set_track_mapping(crate::storage::TrackMapping {
            track_id: "555".to_owned(),
            file_path: file,
        })
        .await
        .unwrap();

    let response = client_via(handle.local_addr())
        .get("http://play.itunes.apple.com/__cider-offline/media/555")
        .header("range", "bytes=3-5")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 206);
    assert_eq!(response.text().await.unwrap(), "345");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn web_playback_for_local_track_should_point_to_media_url() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    // Whether 555 is a Dolby Atmos song is unknown and can't be looked up
    // (no tokens); with Cider's spatial audio off only normal is wanted, so
    // the local file answers (see `crate::routing`).
    handle.set_cider_spatial(crate::routing::CiderSpatial::Off);
    let file = dir.path().join("song.m4a");
    std::fs::write(&file, b"m4a").unwrap();
    handle
        .storage()
        .set_track_mapping(crate::storage::TrackMapping {
            track_id: "555".to_owned(),
            file_path: file,
        })
        .await
        .unwrap();

    let body: serde_json::Value = client_via(handle.local_addr())
        .post("http://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback")
        .body(r#"{"salableAdamId":"555"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        body.pointer("/songList/0/assets/0/URL")
            .and_then(|v| v.as_str()),
        Some(crate::media::media_url("555").as_str())
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn library_should_share_proxy_storage() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    assert!(handle.library().remove_track("missing").await.is_ok());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn forced_offline_should_answer_locally_and_restore_on_release() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    handle.set_force_offline(true);
    let forced = handle.status().online;
    let body: serde_json::Value = client_via(handle.local_addr())
        .get("http://amp-api.music.apple.com/v1/me/library/playlist-folders/p.playlistsroot/children")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.set_force_offline(false);
    assert_eq!(
        (
            forced,
            body.pointer("/meta/total").cloned(),
            handle.status().online
        ),
        (false, Some(serde_json::json!(0)), true)
    );
    handle.shutdown().await.unwrap();
}

// ----- Lyrics and custom audio precedence ---------------------------------------

const CUSTOM_TTML: &str = r#"<tt itunes:timing="Line"><p begin="1">mine</p></tt>"#;
const PROVIDER_TTML: &str = r#"<tt itunes:timing="Line"><p begin="1">netease</p></tt>"#;

async fn get_json(handle: &ProxyHandle, url: &str) -> (u16, serde_json::Value) {
    let response = client_via(handle.local_addr())
        .get(url)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_default())
}

fn ttml(body: &serde_json::Value) -> Option<&str> {
    body.pointer("/data/0/attributes/ttmlLocalizations")
        .and_then(|v| v.as_str())
}

#[tokio::test]
async fn custom_lyrics_should_be_served_online_on_both_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let store = handle.library().lyrics_store();
    store
        .write("555", crate::lyrics::LyricsKind::Syllable, PROVIDER_TTML)
        .await
        .unwrap();
    store.write_custom("555", CUSTOM_TTML).await.unwrap();

    let base = "http://amp-api.music.apple.com/v1/catalog/us/songs/555";
    let line = get_json(&handle, &format!("{base}/lyrics")).await;
    let word = get_json(&handle, &format!("{base}/syllable-lyrics")).await;

    assert_eq!(
        (line.0, ttml(&line.1), word.0, ttml(&word.1)),
        (200, Some(CUSTOM_TTML), 200, Some(CUSTOM_TTML))
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn always_use_downloaded_lyrics_should_apply_live_online() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    handle
        .library()
        .lyrics_store()
        .write("555", crate::lyrics::LyricsKind::Line, PROVIDER_TTML)
        .await
        .unwrap();
    handle.set_prefer_local_lyrics(true);

    let (status, body) = get_json(
        &handle,
        "http://amp-api.music.apple.com/v1/catalog/us/songs/555/lyrics",
    )
    .await;

    assert_eq!(
        (status, ttml(&body), handle.status().counters.served_locally),
        (200, Some(PROVIDER_TTML), 1)
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn offline_lyrics_without_local_file_should_be_404() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    set_online(&handle, false);
    let (status, _) = get_json(
        &handle,
        "http://amp-api.music.apple.com/v1/catalog/us/songs/555/lyrics",
    )
    .await;
    assert_eq!(status, 404);
    handle.shutdown().await.unwrap();
}

async fn play(handle: &ProxyHandle, id: &str) -> Option<String> {
    let body: serde_json::Value = client_via(handle.local_addr())
        .post("http://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback")
        .body(format!(r#"{{"salableAdamId":"{id}"}}"#))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body.pointer("/songList/0/assets/0/URL")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

async fn media_bytes(handle: &ProxyHandle, id: &str) -> String {
    client_via(handle.local_addr())
        .get(format!(
            "http://play.itunes.apple.com/__cider-offline/media/{id}"
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn custom_song_audio_should_be_served_online() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let file = dir.path().join("mine.m4a");
    std::fs::write(&file, b"custom").unwrap();
    let id = handle
        .storage()
        .create_custom_track(crate::storage::NewCustomTrack {
            title: "Mine".to_owned(),
            artist: "Me".to_owned(),
            audio: crate::storage::CustomAudio::LocalCopy(file),
            ..crate::storage::NewCustomTrack::default()
        })
        .await
        .unwrap();

    let url = play(&handle, &id).await;
    let bytes = media_bytes(&handle, &id).await;

    assert_eq!(
        (url, bytes),
        (Some(crate::media::media_url(&id)), "custom".to_owned())
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn audio_override_should_win_over_downloaded_file_online() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    // Whether 555 is a Dolby Atmos song is unknown and can't be looked up
    // (no tokens); with Cider's spatial audio off only normal is wanted, so
    // the local file answers (see `crate::routing`).
    handle.set_cider_spatial(crate::routing::CiderSpatial::Off);
    let downloaded = dir.path().join("downloaded.m4a");
    let mine = dir.path().join("mine.m4a");
    std::fs::write(&downloaded, b"downloaded").unwrap();
    std::fs::write(&mine, b"override").unwrap();
    let storage = handle.storage();
    storage
        .upsert_track(crate::storage::Track::minimal("555", "Song", "Artist"))
        .await
        .unwrap();
    storage
        .set_track_audio(
            "555",
            crate::storage::AudioVariant::Stereo,
            Some(downloaded),
        )
        .await
        .unwrap();
    storage
        .set_track_mapping(crate::storage::TrackMapping {
            track_id: "555".to_owned(),
            file_path: mine,
        })
        .await
        .unwrap();

    let url = play(&handle, "555").await;
    let bytes = media_bytes(&handle, "555").await;

    assert_eq!(
        (url, bytes),
        (Some(crate::media::media_url("555")), "override".to_owned())
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn offline_song_with_local_lyrics_should_claim_lyrics() {
    let dir = tempfile::tempdir().unwrap();
    let handle = start(&dir, Mode::Active, None).await;
    let resource = serde_json::json!({
        "id": "555",
        "type": "songs",
        "attributes": {
            "name": "Song",
            "artistName": "Artist",
            "hasLyrics": false,
            "hasTimeSyncedLyrics": false,
            "contentRating": "explicit",
            "playParams": {"id": "555", "kind": "song"},
        },
    });
    handle
        .storage()
        .upsert_track(crate::storage::Track {
            metadata: Some(resource.to_string()),
            ..crate::storage::Track::minimal("555", "Song", "Artist")
        })
        .await
        .unwrap();
    handle
        .library()
        .lyrics_store()
        .write("555", crate::lyrics::LyricsKind::Line, PROVIDER_TTML)
        .await
        .unwrap();
    set_online(&handle, false);

    let (status, body) = get_json(
        &handle,
        "http://amp-api.music.apple.com/v1/catalog/us/songs/555",
    )
    .await;

    let attributes = |key: &str| body.pointer(&format!("/data/0/attributes/{key}")).cloned();
    assert_eq!(
        (
            status,
            attributes("hasLyrics"),
            attributes("hasTimeSyncedLyrics"),
            attributes("contentRating")
        ),
        (
            200,
            Some(serde_json::json!(true)),
            Some(serde_json::json!(true)),
            Some(serde_json::json!("explicit"))
        )
    );
    handle.shutdown().await.unwrap();
}
