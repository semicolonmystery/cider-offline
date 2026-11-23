//! Tests of the routing rule and of the request flows Cider makes, with the
//! request shapes recorded in the 2026-11-20 log and captures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use http_body_util::BodyExt as _;
use hudsucker::hyper::{Request, StatusCode};
use serde_json::json;
use tokio_util::task::TaskTracker;

use super::*;
use crate::media::MediaResolver;
use crate::playback::LocalPlaybackInterceptor;
use crate::storage::{CatalogUpdate, TrackMetadata};

const NORMAL: AudioVariant = AudioVariant::Stereo;
const SPATIAL: AudioVariant = AudioVariant::Spatial;

fn variants(normal: bool, spatial: bool) -> LocalVariants {
    LocalVariants { normal, spatial }
}

fn when(online: bool, play_other: bool) -> Conditions {
    Conditions { online, play_other }
}

// ----- The rule -----------------------------------------------------------

/// Every combination of online/offline x setting x local versions x the
/// version Cider wants, with the expected source.
#[test]
fn source_matrix_should_follow_the_design() {
    use Source::{Apple, Local, Unavailable};
    let none = variants(false, false);
    let normal = variants(true, false);
    let spatial = variants(false, true);
    let both = variants(true, true);
    #[rustfmt::skip]
    let table = [
        // online, play_other, local, wanted => source
        (true,  false, none,    NORMAL,  Apple),
        (true,  false, none,    SPATIAL, Apple),
        (true,  false, normal,  NORMAL,  Local(NORMAL)),
        (true,  false, normal,  SPATIAL, Apple),
        (true,  false, spatial, NORMAL,  Apple),
        (true,  false, spatial, SPATIAL, Local(SPATIAL)),
        (true,  false, both,    NORMAL,  Local(NORMAL)),
        (true,  false, both,    SPATIAL, Local(SPATIAL)),
        (true,  true,  none,    NORMAL,  Apple),
        (true,  true,  none,    SPATIAL, Apple),
        (true,  true,  normal,  NORMAL,  Local(NORMAL)),
        (true,  true,  normal,  SPATIAL, Local(NORMAL)),
        (true,  true,  spatial, NORMAL,  Local(SPATIAL)),
        (true,  true,  spatial, SPATIAL, Local(SPATIAL)),
        (true,  true,  both,    NORMAL,  Local(NORMAL)),
        (true,  true,  both,    SPATIAL, Local(SPATIAL)),
        (false, false, none,    NORMAL,  Unavailable),
        (false, false, none,    SPATIAL, Unavailable),
        (false, false, normal,  NORMAL,  Local(NORMAL)),
        (false, false, normal,  SPATIAL, Local(NORMAL)),
        (false, false, spatial, NORMAL,  Local(SPATIAL)),
        (false, false, spatial, SPATIAL, Local(SPATIAL)),
        (false, false, both,    NORMAL,  Local(NORMAL)),
        (false, false, both,    SPATIAL, Local(SPATIAL)),
        (false, true,  none,    NORMAL,  Unavailable),
        (false, true,  none,    SPATIAL, Unavailable),
        (false, true,  normal,  NORMAL,  Local(NORMAL)),
        (false, true,  normal,  SPATIAL, Local(NORMAL)),
        (false, true,  spatial, NORMAL,  Local(SPATIAL)),
        (false, true,  spatial, SPATIAL, Local(SPATIAL)),
        (false, true,  both,    NORMAL,  Local(NORMAL)),
        (false, true,  both,    SPATIAL, Local(SPATIAL)),
    ];
    let wrong: Vec<_> = table
        .iter()
        .filter(|(online, play_other, local, wanted, expected)| {
            source_for(*wanted, *local, when(*online, *play_other)) != *expected
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// The `webPlayback` answer for every combination, with Cider's wish known.
#[test]
fn web_playback_matrix_should_follow_the_rule() {
    use WebPlaybackRoute::{Apple, Local, Unavailable};
    let none = variants(false, false);
    let normal = variants(true, false);
    let spatial = variants(false, true);
    let both = variants(true, true);
    #[rustfmt::skip]
    let table = [
        // online, play_other, local, wants spatial => answer
        (true,  false, none,    false, Apple),
        (true,  false, none,    true,  Apple),
        (true,  false, normal,  false, Local(NORMAL)),
        // The restart-bug case: Cider takes the enhanced path and needs
        // Apple's keys.
        (true,  false, normal,  true,  Apple),
        (true,  false, spatial, false, Apple),
        (true,  false, spatial, true,  Local(SPATIAL)),
        (true,  false, both,    false, Local(NORMAL)),
        // Spatial plays through the local playlist; the webPlayback asset
        // (normal path) is the normal file.
        (true,  false, both,    true,  Local(NORMAL)),
        (true,  true,  normal,  true,  Local(NORMAL)),
        (true,  true,  spatial, false, Local(SPATIAL)),
        (false, false, none,    false, Unavailable),
        (false, false, normal,  true,  Local(NORMAL)),
        (false, false, spatial, false, Local(SPATIAL)),
        (false, false, spatial, true,  Local(SPATIAL)),
        (false, true,  both,    true,  Local(NORMAL)),
    ];
    let wrong: Vec<_> = table
        .iter()
        .filter(|(online, play_other, local, wish, expected)| {
            web_playback_route(*local, Some(*wish), when(*online, *play_other)) != *expected
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

#[test]
fn unknown_wish_should_pass_web_playback_to_apple_when_it_matters() {
    assert_eq!(
        web_playback_route(variants(true, false), None, when(true, false)),
        WebPlaybackRoute::Apple
    );
}

#[test]
fn unknown_wish_should_not_matter_with_both_versions_local() {
    assert_eq!(
        web_playback_route(variants(true, true), None, when(true, false)),
        WebPlaybackRoute::Local(NORMAL)
    );
}

#[test]
fn unknown_wish_should_not_matter_offline() {
    assert_eq!(
        web_playback_route(variants(false, true), None, when(false, false)),
        WebPlaybackRoute::Local(SPATIAL)
    );
}

#[test]
fn unknown_wish_should_not_matter_with_the_setting_on() {
    assert!(!web_playback_depends_on_wish(
        variants(true, false),
        when(true, true)
    ));
}

/// The catalog rewrite for every combination, with no `webPlayback` seen.
#[test]
fn asset_urls_matrix_should_follow_the_rule() {
    use AssetUrlsRoute::{Keep, LocalSpatial, Strip};
    let none = variants(false, false);
    let normal = variants(true, false);
    let spatial = variants(false, true);
    let both = variants(true, true);
    #[rustfmt::skip]
    let table = [
        // online, play_other, local, wants spatial => rewrite
        (true,  false, none,    true,  Keep),
        (true,  false, normal,  false, Strip),
        (true,  false, normal,  true,  Keep),
        (true,  false, spatial, false, Keep),
        (true,  false, spatial, true,  LocalSpatial),
        (true,  false, both,    false, Strip),
        (true,  false, both,    true,  LocalSpatial),
        (true,  true,  normal,  true,  Strip),
        (true,  true,  spatial, false, Strip),
        (false, false, none,    true,  Keep),
        (false, false, normal,  true,  Strip),
        (false, false, spatial, false, Strip),
        (false, false, spatial, true,  LocalSpatial),
        (false, false, both,    true,  LocalSpatial),
    ];
    let wrong: Vec<_> = table
        .iter()
        .filter(|(online, play_other, local, wish, expected)| {
            asset_urls_route(*local, *wish, when(*online, *play_other), None) != *expected
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

#[test]
fn local_web_playback_answer_should_keep_cider_off_apples_stream() {
    assert_eq!(
        asset_urls_route(
            variants(true, false),
            true,
            when(true, false),
            Some(WebPlaybackRoute::Local(NORMAL))
        ),
        AssetUrlsRoute::Strip
    );
}

#[test]
fn apple_web_playback_answer_should_keep_apples_stream_urls() {
    assert_eq!(
        asset_urls_route(
            variants(true, false),
            false,
            when(true, false),
            Some(WebPlaybackRoute::Apple)
        ),
        AssetUrlsRoute::Keep
    );
}

#[test]
fn enhanced_request_should_play_a_local_spatial_copy() {
    assert_eq!(
        enhanced_route(variants(true, true), when(true, false), None),
        EnhancedRoute::Local(SPATIAL)
    );
}

#[test]
fn enhanced_request_should_stream_when_only_normal_is_local_by_default() {
    assert_eq!(
        enhanced_route(variants(true, false), when(true, false), None),
        EnhancedRoute::Apple
    );
}

#[test]
fn enhanced_request_should_play_normal_with_the_setting_on() {
    assert_eq!(
        enhanced_route(variants(true, false), when(true, true), None),
        EnhancedRoute::Local(NORMAL)
    );
}

#[test]
fn enhanced_request_should_play_local_after_a_local_web_playback_answer() {
    assert_eq!(
        enhanced_route(
            variants(true, false),
            when(true, false),
            Some(WebPlaybackRoute::Local(NORMAL))
        ),
        EnhancedRoute::Local(NORMAL)
    );
}

#[test]
fn enhanced_request_should_pass_on_with_nothing_local() {
    assert_eq!(
        enhanced_route(variants(false, false), when(false, false), None),
        EnhancedRoute::Apple
    );
}

#[test]
fn cider_spatial_off_should_mean_normal_even_for_atmos_songs() {
    let router = Router::new(
        Arc::new(AtomicBool::new(false)),
        Router::cider_spatial_cell(CiderSpatial::Off),
    );
    assert_eq!(router.wants_spatial(Some(true)), Some(false));
}

#[test]
fn unknown_cider_setting_should_count_as_on() {
    let router = Router::new(
        Arc::new(AtomicBool::new(false)),
        Router::cider_spatial_cell(CiderSpatial::Unknown),
    );
    assert_eq!(
        (router.wants_spatial(Some(true)), router.wants_spatial(None)),
        (Some(true), None)
    );
}

#[test]
fn memory_should_forget_when_full() {
    let mut memory = Memory::default();
    for index in 0..=MEMORY_CAPACITY {
        memory.insert(index.to_string(), index);
    }
    assert_eq!((memory.entries.len(), memory.get("0")), (1, None::<&usize>));
}

// ----- Flows with recorded request shapes ----------------------------------

/// Creepin' (Atmos): catalog id, library id and its enhancedHls master and
/// binaural variant as Cider requested them on 2026-11-20.
const CREEPIN: &str = "1657271974";
const CREEPIN_LIBRARY: &str = "a.1657271974";
const CREEPIN_MASTER: &str = "https://aod.itunes.apple.com/itunes-assets/HLSMusic211/v4/87/c8/06/87c80620-cd16-7d25-9ff0-abfa68e250cf/P1249902232_default.m3u8";
const CREEPIN_VARIANT: &str = "https://aod.itunes.apple.com:443/itunes-assets/HLSMusic211/v4/87/c8/06/87c80620-cd16-7d25-9ff0-abfa68e250cf/P1249902232_A1657271974_audio_en_gr256_mp4a-40-2_bm.m3u8";

/// Which local copies the test store gets.
#[derive(Clone, Copy)]
struct Setup {
    normal: bool,
    spatial: bool,
    play_other: bool,
    cider: CiderSpatial,
}

impl Setup {
    fn new(normal: bool, spatial: bool) -> Self {
        Self {
            normal,
            spatial,
            play_other: false,
            cider: CiderSpatial::On,
        }
    }
}

struct Harness {
    dir: tempfile::TempDir,
    storage: Storage,
    assets: AssetRoutingInterceptor,
    playback: LocalPlaybackInterceptor,
    normal_size: u64,
    spatial_size: u64,
}

/// A store with Creepin' (library row `a.1657271974`, catalog `1657271974`)
/// and the chosen local copies (fragmented AAC files).
async fn harness(setup: Setup) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_in_memory().unwrap();
    storage
        .apply_catalog_update(CatalogUpdate {
            tracks: vec![TrackMetadata {
                id: CREEPIN_LIBRARY.to_owned(),
                catalog_id: Some(CREEPIN.to_owned()),
                title: "Creepin'".to_owned(),
                artist: "Metro Boomin".to_owned(),
                duration_ms: Some(221_520),
                ..TrackMetadata::default()
            }],
            ..CatalogUpdate::default()
        })
        .await
        .unwrap();
    let mut sizes = [0u64; 2];
    for (index, (present, variant, fragments)) in
        [(setup.normal, NORMAL, 3), (setup.spatial, SPATIAL, 5)]
            .into_iter()
            .enumerate()
    {
        if !present {
            continue;
        }
        let path: PathBuf = dir
            .path()
            .join(crate::download::media_file_name(CREEPIN, variant));
        let bytes = crate::hls::tests::sample_file(fragments);
        sizes[index] = u64::try_from(bytes.len()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        storage
            .set_audio_by_any_id(CREEPIN, variant, Some(path))
            .await
            .unwrap();
    }
    let router = Router::new(
        Arc::new(AtomicBool::new(setup.play_other)),
        Router::cider_spatial_cell(setup.cider),
    );
    Harness {
        dir,
        storage: storage.clone(),
        assets: AssetRoutingInterceptor::new(router.clone()),
        playback: LocalPlaybackInterceptor::new(MediaResolver::new(storage), router),
        normal_size: sizes[0],
        spatial_size: sizes[1],
    }
}

/// What the playback interceptor did with a request.
#[derive(Debug)]
enum Outcome {
    /// Passed on (to Apple or the offline responder).
    Passed,
    /// Answered locally.
    Answered(StatusCode, String, Vec<u8>),
}

impl Harness {
    fn exchange(&self, request: &Request<Body>, online: bool) -> Exchange {
        Exchange::new(1, request, online, self.storage.clone(), TaskTracker::new())
    }

    async fn request(&self, request: Request<Body>, online: bool) -> Outcome {
        let mut exchange = self.exchange(&request, online);
        match self.playback.on_request(&mut exchange, request).await {
            RequestOutcome::Continue(_) => Outcome::Passed,
            RequestOutcome::Respond(response) => {
                let status = response.status();
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                Outcome::Answered(status, content_type, body.to_vec())
            }
        }
    }

    /// Cider's `webPlayback` POST for a library play of Creepin'.
    async fn web_playback(&self, online: bool) -> Outcome {
        let body = json!({"universalLibraryId": CREEPIN_LIBRARY, "subscriptionAdamId": CREEPIN});
        let request = Request::builder()
            .method("POST")
            .uri("https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.request(request, online).await
    }

    /// The local media URL a local `webPlayback` answer points at.
    async fn web_playback_url(&self, online: bool) -> Option<String> {
        match self.web_playback(online).await {
            Outcome::Answered(_, _, body) => serde_json::from_slice::<Value>(&body)
                .unwrap()
                .pointer("/songList/0/assets/0/URL")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Outcome::Passed => None,
        }
    }

    async fn get(&self, url: &str, online: bool) -> Outcome {
        let request = Request::builder()
            .uri(url)
            .header("origin", "https://music.apple.com")
            .body(Body::empty())
            .unwrap();
        self.request(request, online).await
    }

    /// Runs the catalog answer Cider gets for
    /// `songs/1657271974?extend=extendedAssetUrls` through the asset routing
    /// and returns the song's `extendedAssetUrls`.
    async fn catalog(&self, online: bool, traits: Value) -> Option<Value> {
        let request = Request::builder()
            .uri(format!(
                "https://amp-api.music.apple.com/v1/catalog/cz/songs/{CREEPIN}?extend=extendedAssetUrls"
            ))
            .body(Body::empty())
            .unwrap();
        let mut exchange = self.exchange(&request, online);
        let document = json!({"data": [{
            "id": CREEPIN,
            "type": "songs",
            "attributes": {
                "name": "Creepin'",
                "audioTraits": traits,
                "playParams": {"id": CREEPIN, "kind": "song"},
                "extendedAssetUrls": {
                    "enhancedHls": CREEPIN_MASTER,
                    "plus": "https://aod.itunes.apple.com/x/plus.m3u8",
                    "lightweight": "https://aod.itunes.apple.com/x/lw.m3u8",
                },
            },
        }]});
        let response = Response::builder()
            .header("content-type", "application/json; charset=utf-8")
            .body(Body::from(document.to_string()))
            .unwrap();
        let response = self.assets.on_response(&mut exchange, response).await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&bytes)
            .unwrap()
            .pointer("/data/0/attributes/extendedAssetUrls")
            .cloned()
    }
}

fn atmos() -> Value {
    json!([
        "atmos",
        "hi-res-lossless",
        "lossless",
        "lossy-stereo",
        "spatial"
    ])
}

fn stereo() -> Value {
    json!(["lossless", "lossy-stereo"])
}

fn body_text(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Answered(_, _, body) => std::str::from_utf8(body).unwrap(),
        Outcome::Passed => "",
    }
}

#[tokio::test]
async fn atmos_play_with_only_normal_local_should_stream_from_apple_with_keys() {
    let h = harness(Setup::new(true, false)).await;
    // Cider shows the song (catalog answer seen) before it plays it.
    h.catalog(true, atmos()).await;
    let web_playback = h.web_playback(true).await;
    let urls = h.catalog(true, atmos()).await;
    let variant = h.get(CREEPIN_VARIANT, true).await;
    assert!(
        matches!(web_playback, Outcome::Passed)
            && urls.is_some_and(|u| u["enhancedHls"] == CREEPIN_MASTER)
            && matches!(variant, Outcome::Passed)
    );
}

#[tokio::test]
async fn atmos_play_with_unknown_traits_should_stream_from_apple() {
    // No catalog answer seen and no catalog client: the wish is unknown.
    let h = harness(Setup::new(true, false)).await;
    assert!(matches!(h.web_playback(true).await, Outcome::Passed));
}

#[tokio::test]
async fn catalog_after_apple_web_playback_should_keep_stream_urls() {
    let h = harness(Setup::new(true, false)).await;
    h.web_playback(true).await;
    // Cider's own catalog answer: the traits now say stereo, but Cider
    // already holds Apple's keys, so both paths work; leave Apple's URLs.
    assert!(h.catalog(true, stereo()).await.is_some());
}

#[tokio::test]
async fn stereo_play_with_normal_local_should_play_the_local_file() {
    let h = harness(Setup::new(true, false)).await;
    h.catalog(true, stereo()).await;
    let url = h.web_playback_url(true).await;
    assert_eq!(
        url.as_deref(),
        Some("https://play.itunes.apple.com/__cider-offline/media/a.1657271974")
    );
}

#[tokio::test]
async fn stereo_play_with_normal_local_should_hide_apples_stream_urls() {
    let h = harness(Setup::new(true, false)).await;
    h.catalog(true, stereo()).await;
    h.web_playback(true).await;
    assert_eq!(h.catalog(true, stereo()).await, None);
}

#[tokio::test]
async fn atmos_play_with_spatial_local_should_point_enhanced_hls_at_the_local_master() {
    let h = harness(Setup::new(true, true)).await;
    h.catalog(true, atmos()).await;
    h.web_playback(true).await;
    let urls = h.catalog(true, atmos()).await;
    assert_eq!(
        urls,
        Some(json!({
            "enhancedHls": "https://play.itunes.apple.com/__cider-offline/hls/1657271974/spatial/master.m3u8"
        }))
    );
}

#[tokio::test]
async fn a_recorded_spatial_file_that_is_gone_should_not_get_a_local_master() {
    let h = harness(Setup::new(true, true)).await;
    std::fs::remove_file(
        h.dir
            .path()
            .join(crate::download::media_file_name(CREEPIN, SPATIAL)),
    )
    .unwrap();
    h.catalog(true, atmos()).await;
    h.web_playback(true).await;
    let urls = h.catalog(true, atmos()).await;
    assert!(
        !urls
            .unwrap_or_default()
            .to_string()
            .contains("__cider-offline"),
        "the local master of a missing file would answer 404"
    );
}

#[tokio::test]
async fn local_master_should_list_the_local_media_playlist() {
    let h = harness(Setup::new(true, true)).await;
    let master = h
        .get(
            "https://play.itunes.apple.com/__cider-offline/hls/1657271974/spatial/master.m3u8",
            true,
        )
        .await;
    assert!(
        body_text(&master).contains(
            "\nhttps://play.itunes.apple.com/__cider-offline/hls/1657271974/spatial/media.m3u8\n"
        ),
        "{master:?}"
    );
}

#[tokio::test]
async fn local_media_playlist_should_point_at_the_spatial_file_without_keys() {
    let h = harness(Setup::new(true, true)).await;
    let media = h
        .get(
            "https://play.itunes.apple.com/__cider-offline/hls/1657271974/spatial/media.m3u8",
            false,
        )
        .await;
    let text = body_text(&media);
    assert!(
        matches!(&media, Outcome::Answered(StatusCode::OK, ct, _) if ct == "application/x-mpegURL")
            && text.contains(
                "https://play.itunes.apple.com/__cider-offline/media/1657271974/spatial\n"
            )
            && !text.contains("EXT-X-KEY"),
        "{media:?}"
    );
}

#[tokio::test]
async fn spatial_media_url_should_serve_the_spatial_file_with_ranges() {
    let h = harness(Setup::new(true, true)).await;
    let request = Request::builder()
        .uri("https://play.itunes.apple.com/__cider-offline/media/1657271974/spatial")
        .header("range", "bytes=0-")
        .body(Body::empty())
        .unwrap();
    let outcome = h.request(request, true).await;
    assert!(
        matches!(&outcome, Outcome::Answered(StatusCode::PARTIAL_CONTENT, _, body)
            if body.len() as u64 == h.spatial_size),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn plain_media_url_should_still_serve_the_normal_file() {
    let h = harness(Setup::new(true, true)).await;
    let outcome = h
        .get(
            "https://play.itunes.apple.com/__cider-offline/media/1657271974",
            true,
        )
        .await;
    assert!(
        matches!(&outcome, Outcome::Answered(StatusCode::OK, _, body)
            if body.len() as u64 == h.normal_size),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn apple_variant_request_should_get_the_local_spatial_playlist() {
    // The browser had Apple's master cached, so Cider asks Apple's variant.
    let h = harness(Setup::new(false, true)).await;
    let outcome = h.get(CREEPIN_VARIANT, true).await;
    assert!(
        body_text(&outcome).contains("/__cider-offline/media/1657271974/spatial\n"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn apple_master_request_should_be_mapped_through_the_catalog_url() {
    let h = harness(Setup::new(false, true)).await;
    h.catalog(true, atmos()).await;
    let outcome = h.get(CREEPIN_MASTER, true).await;
    assert!(
        body_text(&outcome).contains("/__cider-offline/hls/1657271974/spatial/media.m3u8"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn unmapped_apple_master_should_pass_through() {
    let h = harness(Setup::new(false, true)).await;
    assert!(matches!(h.get(CREEPIN_MASTER, true).await, Outcome::Passed));
}

#[tokio::test]
async fn atmos_play_with_only_normal_and_setting_on_should_play_normal_locally() {
    let h = harness(Setup {
        play_other: true,
        ..Setup::new(true, false)
    })
    .await;
    h.catalog(true, atmos()).await;
    let url = h.web_playback_url(true).await;
    let urls = h.catalog(true, atmos()).await;
    assert_eq!(
        (url.as_deref(), urls),
        (
            Some("https://play.itunes.apple.com/__cider-offline/media/a.1657271974"),
            None
        )
    );
}

#[tokio::test]
async fn cached_apple_variant_after_local_answer_should_get_a_local_playlist() {
    // Setting on: webPlayback answered with the normal file (no keys). If
    // Cider still asks Apple's variant (cached master), it must not get
    // Apple's encrypted stream.
    let h = harness(Setup {
        play_other: true,
        ..Setup::new(true, false)
    })
    .await;
    h.catalog(true, atmos()).await;
    h.web_playback(true).await;
    let outcome = h.get(CREEPIN_VARIANT, true).await;
    assert!(
        body_text(&outcome).contains("/__cider-offline/media/1657271974\n"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn stereo_play_with_only_spatial_should_stream_normal_by_default() {
    let h = harness(Setup {
        cider: CiderSpatial::Off,
        ..Setup::new(false, true)
    })
    .await;
    assert!(matches!(h.web_playback(true).await, Outcome::Passed));
}

#[tokio::test]
async fn stereo_play_with_only_spatial_and_setting_on_should_play_spatial() {
    let h = harness(Setup {
        play_other: true,
        cider: CiderSpatial::Off,
        ..Setup::new(false, true)
    })
    .await;
    assert_eq!(
        h.web_playback_url(true).await.as_deref(),
        Some("https://play.itunes.apple.com/__cider-offline/media/a.1657271974/spatial")
    );
}

#[tokio::test]
async fn offline_stereo_play_with_only_spatial_should_play_spatial() {
    let h = harness(Setup {
        cider: CiderSpatial::Off,
        ..Setup::new(false, true)
    })
    .await;
    assert_eq!(
        h.web_playback_url(false).await.as_deref(),
        Some("https://play.itunes.apple.com/__cider-offline/media/a.1657271974/spatial")
    );
}

#[tokio::test]
async fn offline_atmos_play_with_only_normal_should_play_normal_and_strip() {
    let h = harness(Setup::new(true, false)).await;
    let url = h.web_playback_url(false).await;
    let urls = h.catalog(false, atmos()).await;
    assert_eq!(
        (url.as_deref(), urls),
        (
            Some("https://play.itunes.apple.com/__cider-offline/media/a.1657271974"),
            None
        )
    );
}

#[tokio::test]
async fn nothing_local_should_pass_web_playback_through_online() {
    let h = harness(Setup::new(false, false)).await;
    assert!(matches!(h.web_playback(true).await, Outcome::Passed));
}

#[tokio::test]
async fn nothing_local_should_leave_catalog_urls() {
    let h = harness(Setup::new(false, false)).await;
    assert!(h.catalog(true, atmos()).await.is_some());
}

#[tokio::test]
async fn setting_change_should_apply_to_the_next_request() {
    let h = harness(Setup::new(true, false)).await;
    h.catalog(true, atmos()).await;
    let before = h.web_playback(true).await;
    h.playback_router_play_other()
        .store(true, Ordering::Relaxed);
    let after = h.web_playback_url(true).await;
    assert!(matches!(before, Outcome::Passed) && after.is_some());
}

impl Harness {
    fn playback_router_play_other(&self) -> &AtomicBool {
        &self.assets.router.play_other
    }
}

/// Manual check on real data: `CIDER_DB_COPY=<copy of store.sqlite>` and
/// `CIDER_CATALOG_JSON=<a captured catalog answer>` print what each condition
/// does with the answer's songs.
#[tokio::test]
#[ignore = "needs CIDER_DB_COPY and CIDER_CATALOG_JSON"]
async fn real_catalog_answer_routing() {
    let storage = Storage::open_async(std::env::var("CIDER_DB_COPY").unwrap().into())
        .await
        .unwrap();
    let text = std::fs::read_to_string(std::env::var("CIDER_CATALOG_JSON").unwrap()).unwrap();
    for (online, play_other, cider) in [
        (true, false, CiderSpatial::On),
        (true, true, CiderSpatial::On),
        (false, false, CiderSpatial::On),
        (true, false, CiderSpatial::Off),
    ] {
        let router = Router::new(
            Arc::new(AtomicBool::new(play_other)),
            Router::cider_spatial_cell(cider),
        );
        let mut document: Value = serde_json::from_str(&text).unwrap();
        let changes = rewrite_document(&router, &storage, &mut document, online)
            .await
            .unwrap();
        let urls = document.pointer("/data/0/attributes/extendedAssetUrls");
        #[expect(clippy::print_stdout, reason = "manual check output")]
        {
            println!(
                "online={online} play_other={play_other} cider={cider:?}: changes={changes:?} \
                 urls={:?}",
                urls.map(|u| u.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()))
            );
        }
    }
}
