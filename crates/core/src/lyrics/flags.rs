//! Marks songs we have lyrics for as having lyrics.
//!
//! Apple's song resources carry `attributes.hasLyrics` and
//! `hasTimeSyncedLyrics`, and Cider uses them to decide whether to offer
//! lyrics at all. A song Apple has no lyrics for, but that has lyrics saved
//! here (any provider, or the user's own), would otherwise never show them.
//!
//! [`LyricsFlagsInterceptor`] runs last on every Apple API JSON response,
//! upstream or answered locally (offline responder, library injection), and
//! sets `hasLyrics: true` on every song resource (`songs`, `library-songs`,
//! wherever it sits: `data`, `included`, `results`, map lookups'
//! `resources`, relationships) whose id has local lyrics, plus
//! `hasTimeSyncedLyrics: true` when those lyrics are line- or word-timed. It
//! never turns a flag off and touches no other attribute. Lookups use the
//! in-memory index of [`LyricsStore`], so no disk is read per response, and a
//! body without changes is passed on as it came (decoded).

use bytes::Bytes;
use hudsucker::Body;
use hudsucker::hyper::Method;
use serde_json::{Map, Value};

use super::{LyricsQuality, LyricsStore};
use crate::apple;
use crate::interceptor_prelude::*;
use crate::respond::{BufferOutcome, buffer_decoded, content_type_essence};

/// Lyrics-flags interceptor (see the [module docs](self)).
#[derive(Debug, Clone)]
pub struct LyricsFlagsInterceptor {
    store: LyricsStore,
}

impl LyricsFlagsInterceptor {
    /// Interceptor reading the lyrics index of `store`.
    pub fn new(store: LyricsStore) -> Self {
        Self { store }
    }

    /// Applies the rewrite to a parsed document, exactly as done to the
    /// responses passing through. Returns whether anything changed.
    pub fn annotate(&self, document: &mut Value) -> bool {
        annotate(document, &self.store)
    }
}

/// Sets the lyrics flags of every song resource in `document` that has local
/// lyrics in `store`. Returns whether anything changed.
pub(crate) fn annotate(document: &mut Value, store: &LyricsStore) -> bool {
    match document {
        Value::Object(map) => {
            let mut changed = annotate_resource(map, store);
            for value in map.values_mut() {
                changed |= annotate(value, store);
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| annotate(item, store) | changed),
        _ => false,
    }
}

/// Annotates `map` itself when it is a song resource with attributes.
fn annotate_resource(map: &mut Map<String, Value>, store: &LyricsStore) -> bool {
    let is_song = map
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "songs" || kind == "library-songs");
    if !is_song {
        return false;
    }
    let Some(quality) = best_quality(map, store) else {
        return false;
    };
    let Some(attributes) = map.get_mut("attributes").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = set_true(attributes, "hasLyrics");
    if quality >= LyricsQuality::Line {
        changed |= set_true(attributes, "hasTimeSyncedLyrics");
    }
    changed
}

/// Best local lyrics timing among the ids of a song resource: its own id
/// and its play params' catalog / item id (a library song's catalog twin).
fn best_quality(map: &Map<String, Value>, store: &LyricsStore) -> Option<LyricsQuality> {
    let play_params = map.get("attributes").and_then(|a| a.get("playParams"));
    [
        map.get("id"),
        play_params.and_then(|p| p.get("catalogId")),
        play_params.and_then(|p| p.get("id")),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .filter_map(|id| store.quality(id))
    .max()
}

/// Sets `attributes[key] = true` unless it already is; never sets `false`.
fn set_true(attributes: &mut Map<String, Value>, key: &str) -> bool {
    if attributes.get(key).and_then(Value::as_bool) == Some(true) {
        return false;
    }
    attributes.insert(key.to_owned(), Value::Bool(true));
    true
}

/// Whether `body` may contain a song resource (`"songs"` / `"library-songs"`),
/// so bodies without any are not parsed.
fn mentions_songs(body: &[u8]) -> bool {
    body.windows(6).any(|window| window == b"songs\"")
}

#[async_trait]
impl Interceptor for LyricsFlagsInterceptor {
    fn name(&self) -> &'static str {
        "lyrics-flags"
    }

    async fn on_response(
        &self,
        exchange: &mut Exchange,
        response: Response<Body>,
    ) -> Response<Body> {
        if *exchange.method() != Method::GET
            || !apple::is_api_host(exchange.host())
            || !response.status().is_success()
            || self.store.index_is_empty()
        {
            return response;
        }
        let is_json = content_type_essence(response.headers()).is_some_and(|t| t.ends_with("json"));
        if !is_json {
            return response;
        }
        let buffered = match buffer_decoded(response).await {
            BufferOutcome::Buffered(buffered) => buffered,
            BufferOutcome::PassThrough(response) => return response,
        };
        if !mentions_songs(&buffered.body) {
            return buffered.into_response();
        }
        let Ok(mut document) = serde_json::from_slice::<Value>(&buffered.body) else {
            return buffered.into_response();
        };
        if annotate(&mut document, &self.store) {
            tracing::debug!(id = exchange.id(), "set lyrics flags from local lyrics");
            buffered.into_response_with(Bytes::from(document.to_string()))
        } else {
            buffered.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use http_body_util::BodyExt as _;
    use hudsucker::hyper::Request;
    use serde_json::json;
    use tokio_util::task::TaskTracker;

    use super::*;
    use crate::lyrics::LyricsKind;
    use crate::storage::Storage;

    const LINE: &str = r#"<tt itunes:timing="Line"><p begin="1">a</p></tt>"#;
    const PLAIN: &str = r#"<tt itunes:timing="None"><p>a</p></tt>"#;

    async fn store_with(entries: &[(&str, &str)]) -> (tempfile::TempDir, LyricsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LyricsStore::new(dir.path().to_path_buf());
        for (id, ttml) in entries {
            store.write(id, LyricsKind::Line, ttml).await.unwrap();
        }
        (dir, store)
    }

    fn song(id: &str, kind: &str, has: bool, synced: bool) -> Value {
        json!({
            "id": id,
            "type": kind,
            "attributes": {
                "name": "x",
                "contentRating": "explicit",
                "hasLyrics": has,
                "hasTimeSyncedLyrics": synced,
            },
        })
    }

    #[tokio::test]
    async fn catalog_song_with_local_synced_lyrics_should_get_both_flags() {
        let (_dir, store) = store_with(&[("1", LINE)]).await;
        let mut doc = json!({"data": [song("1", "songs", false, false)]});
        annotate(&mut doc, &store);
        assert_eq!(
            (
                doc.pointer("/data/0/attributes/hasLyrics"),
                doc.pointer("/data/0/attributes/hasTimeSyncedLyrics")
            ),
            (Some(&json!(true)), Some(&json!(true)))
        );
    }

    #[tokio::test]
    async fn plain_lyrics_should_not_claim_time_sync() {
        let (_dir, store) = store_with(&[("1", PLAIN)]).await;
        let mut doc = json!({"data": [song("1", "songs", false, false)]});
        annotate(&mut doc, &store);
        assert_eq!(
            (
                doc.pointer("/data/0/attributes/hasLyrics"),
                doc.pointer("/data/0/attributes/hasTimeSyncedLyrics")
            ),
            (Some(&json!(true)), Some(&json!(false)))
        );
    }

    #[tokio::test]
    async fn flags_should_never_turn_false() {
        let (_dir, store) = store_with(&[("1", PLAIN)]).await;
        let mut doc = json!({"data": [song("1", "songs", true, true)]});
        let changed = annotate(&mut doc, &store);
        assert_eq!(
            (
                changed,
                doc.pointer("/data/0/attributes/hasTimeSyncedLyrics")
            ),
            (false, Some(&json!(true)))
        );
    }

    #[tokio::test]
    async fn other_attributes_should_stay_untouched() {
        let (_dir, store) = store_with(&[("1", LINE)]).await;
        let mut doc = json!({"data": [song("1", "songs", false, false)]});
        annotate(&mut doc, &store);
        assert_eq!(
            doc.pointer("/data/0/attributes/contentRating"),
            Some(&json!("explicit"))
        );
    }

    #[tokio::test]
    async fn library_song_should_match_by_catalog_id() {
        let (_dir, store) = store_with(&[("1", LINE)]).await;
        let mut resource = song("i.abc", "library-songs", false, false);
        resource["attributes"]["playParams"] = json!({"id": "i.abc", "catalogId": "1"});
        let mut doc = json!({"data": [resource]});
        annotate(&mut doc, &store);
        assert_eq!(
            doc.pointer("/data/0/attributes/hasLyrics"),
            Some(&json!(true))
        );
    }

    #[tokio::test]
    async fn nested_resources_should_be_annotated() {
        let (_dir, store) = store_with(&[("1", LINE), ("2", LINE), ("3", LINE)]).await;
        let mut doc = json!({
            "results": {"song": {"data": [song("1", "songs", false, false)]}},
            "resources": {"songs": {"2": song("2", "songs", false, false)}},
            "data": [{"id": "p.1", "type": "library-playlists",
                "relationships": {"tracks": {"data": [song("3", "songs", false, false)]}}}],
        });
        annotate(&mut doc, &store);
        let flags: Vec<_> = [
            "/results/song/data/0/attributes/hasLyrics",
            "/resources/songs/2/attributes/hasLyrics",
            "/data/0/relationships/tracks/data/0/attributes/hasLyrics",
        ]
        .iter()
        .map(|p| doc.pointer(p).cloned())
        .collect();
        assert_eq!(flags, vec![Some(json!(true)); 3]);
    }

    #[tokio::test]
    async fn songs_without_local_lyrics_should_be_unchanged() {
        let (_dir, store) = store_with(&[("1", LINE)]).await;
        let mut doc = json!({"data": [song("9", "songs", false, false)]});
        assert!(!annotate(&mut doc, &store));
    }

    #[tokio::test]
    async fn custom_lyrics_should_count() {
        let (_dir, store) = store_with(&[]).await;
        store.write_custom("local-1", LINE).await.unwrap();
        let mut doc = json!({"data": [song("local-1", "songs", false, false)]});
        annotate(&mut doc, &store);
        assert_eq!(
            doc.pointer("/data/0/attributes/hasTimeSyncedLyrics"),
            Some(&json!(true))
        );
    }

    async fn respond_through(
        store: LyricsStore,
        response: Response<Body>,
    ) -> (Option<String>, Value) {
        let request = Request::builder()
            .uri("https://amp-api.music.apple.com/v1/catalog/us/songs?ids=1")
            .body(Body::empty())
            .unwrap();
        let mut exchange = Exchange::new(
            1,
            &request,
            true,
            Storage::open_in_memory().unwrap(),
            TaskTracker::new(),
        );
        let response = LyricsFlagsInterceptor::new(store)
            .on_response(&mut exchange, response)
            .await;
        let encoding = response
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (encoding, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn gzip_upstream_body_should_be_decoded_and_rewritten() {
        let (_dir, store) = store_with(&[("1", LINE)]).await;
        let body = json!({"data": [song("1", "songs", false, false)]}).to_string();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body.as_bytes()).unwrap();
        let response = Response::builder()
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(Body::from(encoder.finish().unwrap()))
            .unwrap();
        let (encoding, doc) = respond_through(store, response).await;
        assert_eq!(
            (encoding, doc.pointer("/data/0/attributes/hasLyrics")),
            (None, Some(&json!(true)))
        );
    }
}
