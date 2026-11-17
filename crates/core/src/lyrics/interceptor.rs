//! Saves Apple lyrics seen through the proxy and answers lyrics requests
//! from local TTML.
//!
//! Precedence, for `songs/{id}/lyrics` and `songs/{id}/syllable-lyrics`:
//!
//! 1. The user's custom lyrics (and every custom song, `local-…`) are always
//!    answered locally, online and offline; Apple is never asked.
//! 2. Offline: local TTML, else `404`.
//! 3. Online with "Always use downloaded lyrics" on: local TTML when there
//!    is any, else Apple.
//! 4. Online otherwise: Apple; local TTML only on a non-2xx answer or an
//!    upstream error. Apple's `2xx` lyrics are saved (replacing earlier
//!    provider lyrics of that kind) unless the song has custom lyrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hudsucker::Body;
use hudsucker::hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};

use super::{LyricsKind, LyricsStore};
use crate::apple;
use crate::interceptor_prelude::*;
use crate::respond::{self, buffer_decoded};
use crate::storage::{CUSTOM_ID_PREFIX, LyricsSource};

/// Lyrics target of the current exchange, kept for the response hook.
#[derive(Debug, Clone)]
struct Target {
    id: String,
    kind: LyricsKind,
}

/// Lyrics interceptor (see the [module docs](super)).
#[derive(Debug, Clone)]
pub struct LyricsInterceptor {
    store: LyricsStore,
    prefer_local: Arc<AtomicBool>,
}

impl LyricsInterceptor {
    /// Interceptor using `store` for local TTML. While `prefer_local` is set
    /// ("Always use downloaded lyrics"), local lyrics are served online too
    /// without asking Apple; it is read on every request.
    pub fn new(store: LyricsStore, prefer_local: Arc<AtomicBool>) -> Self {
        Self {
            store,
            prefer_local,
        }
    }

    /// Whether `target` must be answered locally without asking Apple.
    async fn local_only(&self, exchange: &Exchange, target: &Target) -> bool {
        !exchange.online()
            || target.id.starts_with(CUSTOM_ID_PREFIX)
            || self.store.has_custom(&target.id).await
    }

    async fn local_response(&self, exchange: &Exchange, target: &Target) -> Option<Response<Body>> {
        let (ttml, _) = self.store.read(&target.id, target.kind).await?;
        Some(respond::json(
            StatusCode::OK,
            &lyrics_document(&target.id, target.kind, &ttml),
            exchange.request_headers(),
            "lyrics",
        ))
    }
}

/// Apple-shaped lyrics document wrapping `ttml`.
///
/// The TTML is emitted under `attributes.ttmlLocalizations` (a bare string),
/// the key Apple uses when asked with `extend=ttmlLocalizations` (as Cider
/// does for `syllable-lyrics`), and under `attributes.ttml`, the key Apple
/// uses without it (Cider's `lyrics` requests).
pub(crate) fn lyrics_document(id: &str, kind: LyricsKind, ttml: &str) -> Value {
    json!({
        "data": [{
            "id": id,
            "type": kind.resource_type(),
            "attributes": {
                "ttmlLocalizations": ttml,
                "ttml": ttml,
                "playParams": {
                    "id": id,
                    "catalogId": id,
                    "kind": "lyric",
                    "displayType": match kind { LyricsKind::Line => 2, LyricsKind::Syllable => 3 },
                },
            },
        }],
    })
}

/// Extracts `data[0].attributes.ttmlLocalizations` from an Apple lyrics response.
///
/// Real responses store the TTML as a bare string there; as a fallback, if a
/// locale-keyed object is ever seen, the first string value is used.
pub(crate) fn ttml_of(document: &Value) -> Option<&str> {
    let value = document.pointer("/data/0/attributes/ttmlLocalizations")?;
    let ttml = match value {
        Value::String(ttml) => ttml.as_str(),
        Value::Object(map) => map.values().find_map(Value::as_str)?,
        _ => return None,
    };
    (!ttml.trim().is_empty()).then_some(ttml)
}

/// TTML of an Apple lyrics document: `ttmlLocalizations` (requested with
/// `extend=ttmlLocalizations`), else plain `attributes.ttml` (Apple's answer
/// without it).
pub(crate) fn apple_ttml(document: &Value) -> Option<&str> {
    ttml_of(document).or_else(|| {
        document
            .pointer("/data/0/attributes/ttml")
            .and_then(Value::as_str)
            .filter(|ttml| !ttml.trim().is_empty())
    })
}

fn not_found(exchange: &Exchange) -> Response<Body> {
    respond::json(
        StatusCode::NOT_FOUND,
        &json!({"errors": [{"status": "404", "code": "40400", "title": "No Lyrics"}]}),
        exchange.request_headers(),
        "lyrics",
    )
}

#[async_trait]
impl Interceptor for LyricsInterceptor {
    fn name(&self) -> &'static str {
        "lyrics"
    }

    async fn on_request(&self, exchange: &mut Exchange, request: Request<Body>) -> RequestOutcome {
        if *exchange.method() != Method::GET || !apple::is_api_host(exchange.host()) {
            return RequestOutcome::Continue(request);
        }
        let Some((id, kind)) = apple::lyrics_target(exchange.uri().path()) else {
            return RequestOutcome::Continue(request);
        };
        let target = Target { id, kind };
        if self.local_only(exchange, &target).await {
            let response = match self.local_response(exchange, &target).await {
                Some(response) => response,
                None => not_found(exchange),
            };
            return RequestOutcome::Respond(response);
        }
        if self.prefer_local.load(Ordering::Relaxed)
            && let Some(response) = self.local_response(exchange, &target).await
        {
            tracing::debug!(id = %target.id, "serving downloaded lyrics instead of Apple's");
            return RequestOutcome::Respond(response);
        }
        exchange.extensions_mut().insert(target);
        RequestOutcome::Continue(request)
    }

    async fn on_response(
        &self,
        exchange: &mut Exchange,
        response: Response<Body>,
    ) -> Response<Body> {
        if exchange.served_locally() {
            return response;
        }
        let Some(target) = exchange.extensions().get::<Target>().cloned() else {
            return response;
        };

        if !response.status().is_success() {
            // Apple has no lyrics (or failed): use ours if we have them.
            if let Some(local) = self.local_response(exchange, &target).await {
                exchange.mark_served_locally();
                return local;
            }
            return response;
        }

        let buffered = match buffer_decoded(response).await {
            Ok(Ok(buffered)) => buffered,
            Ok(Err(untouched)) => return untouched,
            Err(failed) => return failed,
        };
        if let Some(ttml) = serde_json::from_slice::<Value>(&buffered.body)
            .ok()
            .as_ref()
            .and_then(apple_ttml)
            .map(str::to_owned)
        {
            let store = self.store.clone();
            let storage = exchange.storage().clone();
            exchange.spawn(async move {
                // Custom lyrics are the user's choice: never relabel them as
                // Apple's (their file is separate, so it stays untouched).
                if store.has_custom(&target.id).await {
                    tracing::debug!(id = %target.id, "kept custom lyrics over Apple's");
                    return;
                }
                if let Err(error) = store.write(&target.id, target.kind, &ttml).await {
                    tracing::warn!(id = %target.id, %error, "failed to save Apple lyrics");
                    return;
                }
                let quality = crate::lyrics::ttml_quality(&ttml);
                if let Err(error) = storage
                    .set_lyrics_state(&target.id, LyricsSource::Apple, quality)
                    .await
                {
                    tracing::warn!(id = %target.id, %error, "failed to update lyrics source");
                }
                tracing::debug!(id = %target.id, "saved Apple lyrics");
            });
        }
        buffered.into_response()
    }

    async fn on_upstream_error(&self, exchange: &mut Exchange) -> Option<Response<Body>> {
        let target = exchange.extensions().get::<Target>().cloned()?;
        Some(match self.local_response(exchange, &target).await {
            Some(response) => response,
            None => not_found(exchange),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_should_roundtrip_ttml() {
        let doc = lyrics_document("1", LyricsKind::Syllable, "<tt/>");
        assert_eq!(ttml_of(&doc), Some("<tt/>"));
    }

    #[test]
    fn document_type_should_follow_kind() {
        let doc = lyrics_document("1", LyricsKind::Syllable, "<tt/>");
        assert_eq!(doc.pointer("/data/0/type"), Some(&json!("syllable-lyrics")));
    }

    // ----- Request / response flows ------------------------------------------

    use crate::storage::{LyricsQuality, Storage, Track};
    use http_body_util::BodyExt as _;
    use tokio_util::task::TaskTracker;

    const APPLE_TTML: &str =
        r#"<tt itunes:timing="Word"><p begin="1"><span begin="1">apple</span></p></tt>"#;
    const PROVIDER_TTML: &str = r#"<tt itunes:timing="Line"><p begin="1">netease</p></tt>"#;
    const CUSTOM_TTML: &str = r#"<tt itunes:timing="Line"><p begin="1">mine</p></tt>"#;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: LyricsStore,
        storage: Storage,
        prefer_local: Arc<AtomicBool>,
        interceptor: LyricsInterceptor,
        tasks: TaskTracker,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = LyricsStore::new(dir.path().to_path_buf());
            let storage = Storage::open_in_memory().unwrap();
            storage
                .upsert_track(Track::minimal("1", "Song", "Artist"))
                .await
                .unwrap();
            let prefer_local = Arc::new(AtomicBool::new(false));
            let interceptor = LyricsInterceptor::new(store.clone(), Arc::clone(&prefer_local));
            Self {
                _dir: dir,
                store,
                storage,
                prefer_local,
                interceptor,
                tasks: TaskTracker::new(),
            }
        }

        fn exchange(&self, id: &str, endpoint: &str, online: bool) -> (Exchange, Request<Body>) {
            let request = Request::builder()
                .uri(format!(
                    "https://amp-api.music.apple.com/v1/catalog/us/songs/{id}/{endpoint}"
                ))
                .body(Body::empty())
                .unwrap();
            let exchange = Exchange::new(
                1,
                &request,
                online,
                self.storage.clone(),
                self.tasks.clone(),
            );
            (exchange, request)
        }

        /// Runs the request hook; `Some` when answered locally.
        async fn request(&self, id: &str, endpoint: &str, online: bool) -> Option<(u16, Value)> {
            let (mut exchange, request) = self.exchange(id, endpoint, online);
            match self.interceptor.on_request(&mut exchange, request).await {
                RequestOutcome::Respond(response) => Some(read(response).await),
                RequestOutcome::Continue(_) => None,
            }
        }

        /// Runs a full online exchange with Apple answering `status` / `body`.
        async fn online_with_apple(&self, id: &str, status: u16, body: &Value) -> (u16, Value) {
            let (mut exchange, request) = self.exchange(id, "syllable-lyrics", true);
            let RequestOutcome::Continue(_) =
                self.interceptor.on_request(&mut exchange, request).await
            else {
                panic!("expected the request to go to Apple");
            };
            self.apple_answers(&mut exchange, status, body).await
        }

        async fn apple_answers(
            &self,
            exchange: &mut Exchange,
            status: u16,
            body: &Value,
        ) -> (u16, Value) {
            let response = Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let response = self.interceptor.on_response(exchange, response).await;
            let answer = read(response).await;
            self.tasks.close();
            self.tasks.wait().await;
            self.tasks.reopen();
            answer
        }
    }

    async fn read(response: Response<Body>) -> (u16, Value) {
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn offline_should_serve_local_lyrics_in_apple_shape() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Line, PROVIDER_TTML)
            .await
            .unwrap();
        let (status, body) = fx.request("1", "syllable-lyrics", false).await.unwrap();
        assert_eq!(
            (status, ttml_of(&body), body.pointer("/data/0/type")),
            (200, Some(PROVIDER_TTML), Some(&json!("syllable-lyrics")))
        );
    }

    #[tokio::test]
    async fn offline_without_lyrics_should_answer_404() {
        let fx = Fixture::new().await;
        let (status, _) = fx.request("1", "lyrics", false).await.unwrap();
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn online_with_setting_off_should_ask_apple_despite_local_lyrics() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Line, PROVIDER_TTML)
            .await
            .unwrap();
        assert!(fx.request("1", "lyrics", true).await.is_none());
    }

    #[tokio::test]
    async fn online_with_setting_on_should_serve_local_lyrics() {
        let fx = Fixture::new().await;
        fx.prefer_local.store(true, Ordering::Relaxed);
        fx.store
            .write("1", LyricsKind::Line, PROVIDER_TTML)
            .await
            .unwrap();
        let (status, body) = fx.request("1", "lyrics", true).await.unwrap();
        assert_eq!((status, ttml_of(&body)), (200, Some(PROVIDER_TTML)));
    }

    #[tokio::test]
    async fn online_with_setting_on_without_local_lyrics_should_ask_apple() {
        let fx = Fixture::new().await;
        fx.prefer_local.store(true, Ordering::Relaxed);
        assert!(fx.request("1", "lyrics", true).await.is_none());
    }

    #[tokio::test]
    async fn setting_should_apply_without_rebuilding_the_interceptor() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Line, PROVIDER_TTML)
            .await
            .unwrap();
        let before = fx.request("1", "lyrics", true).await.is_some();
        fx.prefer_local.store(true, Ordering::Relaxed);
        let after = fx.request("1", "lyrics", true).await.is_some();
        assert_eq!((before, after), (false, true));
    }

    #[tokio::test]
    async fn custom_lyrics_should_win_online_on_both_endpoints() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Syllable, APPLE_TTML)
            .await
            .unwrap();
        fx.store.write_custom("1", CUSTOM_TTML).await.unwrap();
        let line = fx.request("1", "lyrics", true).await.unwrap();
        let word = fx.request("1", "syllable-lyrics", true).await.unwrap();
        assert_eq!(
            (ttml_of(&line.1), ttml_of(&word.1)),
            (Some(CUSTOM_TTML), Some(CUSTOM_TTML))
        );
    }

    #[tokio::test]
    async fn custom_lyrics_should_win_offline() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Syllable, APPLE_TTML)
            .await
            .unwrap();
        fx.store.write_custom("1", CUSTOM_TTML).await.unwrap();
        let (_, body) = fx.request("1", "syllable-lyrics", false).await.unwrap();
        assert_eq!(ttml_of(&body), Some(CUSTOM_TTML));
    }

    #[tokio::test]
    async fn custom_song_should_never_ask_apple() {
        let fx = Fixture::new().await;
        let (status, _) = fx.request("local-7", "lyrics", true).await.unwrap();
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn apple_lyrics_should_be_saved_and_marked_apple() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Syllable, PROVIDER_TTML)
            .await
            .unwrap();
        let apple = lyrics_document("1", LyricsKind::Syllable, APPLE_TTML);
        let (status, _) = fx.online_with_apple("1", 200, &apple).await;
        let saved = fx
            .store
            .read("1", LyricsKind::Syllable)
            .await
            .map(|(t, _)| t);
        let source = fx
            .storage
            .get_track("1")
            .await
            .unwrap()
            .unwrap()
            .lyrics_source;
        assert_eq!(
            (status, saved.as_deref(), source),
            (200, Some(APPLE_TTML), LyricsSource::Apple)
        );
    }

    #[tokio::test]
    async fn apple_lyrics_should_never_overwrite_custom_lyrics() {
        let fx = Fixture::new().await;
        // Custom lyrics imported while the request was in flight to Apple.
        let (mut exchange, request) = fx.exchange("1", "lyrics", true);
        let RequestOutcome::Continue(_) = fx.interceptor.on_request(&mut exchange, request).await
        else {
            panic!("expected the request to go to Apple");
        };
        fx.store.write_custom("1", CUSTOM_TTML).await.unwrap();
        fx.storage
            .set_lyrics_state("1", LyricsSource::Custom, LyricsQuality::Line)
            .await
            .unwrap();
        let apple = lyrics_document("1", LyricsKind::Line, APPLE_TTML);
        fx.apple_answers(&mut exchange, 200, &apple).await;
        let served = fx.store.read("1", LyricsKind::Line).await.map(|(t, _)| t);
        let source = fx
            .storage
            .get_track("1")
            .await
            .unwrap()
            .unwrap()
            .lyrics_source;
        assert_eq!(
            (served.as_deref(), source),
            (Some(CUSTOM_TTML), LyricsSource::Custom)
        );
    }

    #[tokio::test]
    async fn apple_line_lyrics_without_extend_should_be_saved() {
        let fx = Fixture::new().await;
        // Cider's `lyrics` request has no `extend=ttmlLocalizations`, so
        // Apple answers with plain `attributes.ttml`.
        let apple = json!({"data": [{"id": "1", "type": "lyrics",
            "attributes": {"ttml": PROVIDER_TTML}}]});
        fx.online_with_apple("1", 200, &apple).await;
        let saved = fx
            .store
            .read_provider("1", LyricsKind::Syllable)
            .await
            .map(|(t, _)| t);
        assert_eq!(saved.as_deref(), Some(PROVIDER_TTML));
    }

    #[test]
    fn document_should_carry_both_ttml_keys() {
        let doc = lyrics_document("1", LyricsKind::Line, "<tt/>");
        assert_eq!(
            doc.pointer("/data/0/attributes/ttml"),
            Some(&json!("<tt/>"))
        );
    }

    #[tokio::test]
    async fn apple_404_should_fall_back_to_local_lyrics() {
        let fx = Fixture::new().await;
        fx.store
            .write("1", LyricsKind::Line, PROVIDER_TTML)
            .await
            .unwrap();
        let (status, body) = fx.online_with_apple("1", 404, &json!({"errors": []})).await;
        assert_eq!((status, ttml_of(&body)), (200, Some(PROVIDER_TTML)));
    }
}
