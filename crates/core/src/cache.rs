//! Response cache: stores successful Apple API JSON and artwork while online,
//! and merges library metadata from the JSON into the local store.

use bytes::Bytes;
use hudsucker::Body;
use hudsucker::hyper::{Method, StatusCode, Uri};
use serde_json::Value;

use crate::apple;
use crate::artwork::{ArtworkStore, MAX_ARTWORK_BYTES};
use crate::catalog;
use crate::interceptor_prelude::*;
use crate::respond::{self, Buffered, buffer_decoded, content_length, content_type_essence};
use crate::storage::{CacheEntry, Storage};

/// Settings key holding the last `/v1/me/storefront` response body.
pub(crate) const STOREFRONT_SETTING: &str = "storefront_response";

/// Largest JSON body that is cached.
const MAX_JSON_BYTES: usize = 32 * 1024 * 1024;

/// What a response would be cached as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cacheable {
    /// Apple API JSON, stored in the HTTP cache.
    Json,
    /// Artwork image, stored as a file.
    Artwork,
}

/// Decides whether a response is cached (pure; see [`ResponseCacheInterceptor`]).
pub(crate) fn classify(
    method: &Method,
    uri: &Uri,
    status: StatusCode,
    content_type: Option<&str>,
    length: Option<u64>,
) -> Option<Cacheable> {
    if *method != Method::GET || status != StatusCode::OK {
        return None;
    }
    let host = uri.host()?;
    let content_type = content_type?;
    if apple::is_api_host(host) && !apple::is_sensitive_path(uri.path()) {
        let json = content_type.ends_with("/json") || content_type.ends_with("+json");
        return json.then_some(Cacheable::Json);
    }
    if apple::is_artwork_host(host) && content_type.starts_with("image/") {
        return length
            .is_some_and(|l| l <= MAX_ARTWORK_BYTES)
            .then_some(Cacheable::Artwork);
    }
    None
}

/// Caches successful `GET` responses of the Apple JSON API (not auth, token,
/// license or playback endpoints) and `*.mzstatic.com` artwork, decoded.
/// Runs only on upstream responses (never on locally produced ones).
#[derive(Debug, Clone)]
pub struct ResponseCacheInterceptor {
    artwork: ArtworkStore,
}

impl ResponseCacheInterceptor {
    pub(crate) fn new(artwork: ArtworkStore) -> Self {
        Self { artwork }
    }
}

fn entry_from(exchange: &Exchange, key: String, buffered: &Buffered) -> CacheEntry {
    CacheEntry {
        key,
        method: exchange.method().as_str().to_owned(),
        url: exchange.uri().to_string(),
        status: buffered.parts.status.as_u16(),
        headers: buffered
            .parts
            .headers
            .iter()
            .filter(|(name, _)| respond::is_replayable_header(name.as_str()))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_owned(), v.to_owned()))
            })
            .collect(),
        body: buffered.body.to_vec(),
        stored_at: now_unix(),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or_default()
}

/// Stores a JSON response and merges its metadata (runs in the background).
async fn persist_json(storage: Storage, entry: CacheEntry, uri: Uri) {
    let Ok(document) = serde_json::from_slice::<Value>(&entry.body) else {
        tracing::debug!(url = %entry.url, "not caching invalid JSON");
        return;
    };
    if apple::is_my_storefront_path(uri.path())
        && document.pointer("/data/0/id").is_some()
        && let Err(error) = storage
            .set_setting(STOREFRONT_SETTING, document.to_string())
            .await
    {
        tracing::warn!(%error, "failed to persist storefront");
    }
    let update = catalog::extract(&document, &uri);
    if let Err(error) = storage.put_cache_entry(entry).await {
        tracing::warn!(%error, "failed to cache response");
    }
    if !update.is_empty()
        && let Err(error) = storage.apply_catalog_update(update).await
    {
        tracing::warn!(%error, "failed to merge library metadata");
    }
}

#[async_trait]
impl Interceptor for ResponseCacheInterceptor {
    fn name(&self) -> &'static str {
        "response-cache"
    }

    async fn on_response(
        &self,
        exchange: &mut Exchange,
        response: Response<Body>,
    ) -> Response<Body> {
        if exchange.served_locally() {
            return response;
        }
        let content_type = content_type_essence(response.headers());
        let Some(kind) = classify(
            exchange.method(),
            exchange.uri(),
            response.status(),
            content_type.as_deref(),
            content_length(response.headers()),
        ) else {
            return response;
        };
        let Some(key) = exchange.cache_key() else {
            return response;
        };
        let buffered = match buffer_decoded(response).await {
            Ok(Ok(buffered)) => buffered,
            Ok(Err(untouched)) => return untouched,
            Err(failed) => return failed,
        };
        match kind {
            Cacheable::Json if buffered.body.len() <= MAX_JSON_BYTES => {
                let entry = entry_from(exchange, key, &buffered);
                exchange.spawn(persist_json(
                    exchange.storage().clone(),
                    entry,
                    exchange.uri().clone(),
                ));
            }
            Cacheable::Json => {}
            Cacheable::Artwork => {
                let store = self.artwork.clone();
                let uri = exchange.uri().clone();
                let body: Bytes = buffered.body.clone();
                let content_type = content_type.unwrap_or_default();
                exchange.spawn(async move {
                    if let Err(error) = store.store(&uri, &content_type, body).await {
                        tracing::warn!(%error, "failed to store artwork");
                    }
                });
            }
        }
        buffered.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn classify_get(url: &str, content_type: &str) -> Option<Cacheable> {
        classify(
            &Method::GET,
            &uri(url),
            StatusCode::OK,
            Some(content_type),
            Some(100),
        )
    }

    #[test]
    fn catalog_json_should_be_cached() {
        assert_eq!(
            classify_get(
                "https://amp-api.music.apple.com/v1/catalog/us/songs/1",
                "application/json"
            ),
            Some(Cacheable::Json)
        );
    }

    #[test]
    fn license_endpoint_should_not_be_cached() {
        assert_eq!(
            classify_get(
                "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense",
                "application/json"
            ),
            None
        );
    }

    #[test]
    fn token_path_should_not_be_cached() {
        assert_eq!(
            classify_get(
                "https://amp-api.music.apple.com/v1/me/user-token",
                "application/json"
            ),
            None
        );
    }

    #[test]
    fn artwork_should_be_cached() {
        assert_eq!(
            classify_get(
                "https://is1-ssl.mzstatic.com/image/thumb/a.jpg/300x300bb.jpg",
                "image/jpeg"
            ),
            Some(Cacheable::Artwork)
        );
    }

    #[test]
    fn artwork_without_length_should_not_be_cached() {
        assert_eq!(
            classify(
                &Method::GET,
                &uri("https://is1-ssl.mzstatic.com/a/1x1bb.jpg"),
                StatusCode::OK,
                Some("image/jpeg"),
                None
            ),
            None
        );
    }

    #[test]
    fn post_should_not_be_cached() {
        assert_eq!(
            classify(
                &Method::POST,
                &uri("https://amp-api.music.apple.com/v1/me/library"),
                StatusCode::OK,
                Some("application/json"),
                Some(1)
            ),
            None
        );
    }

    #[test]
    fn error_status_should_not_be_cached() {
        assert_eq!(
            classify(
                &Method::GET,
                &uri("https://amp-api.music.apple.com/v1/catalog/us/songs/1"),
                StatusCode::NOT_FOUND,
                Some("application/json"),
                Some(1)
            ),
            None
        );
    }

    #[tokio::test]
    async fn persist_should_store_storefront_and_entry() {
        let storage = Storage::open_in_memory().unwrap();
        let url = "https://amp-api.music.apple.com/v1/me/storefront";
        let entry = CacheEntry {
            key: format!("GET {url}"),
            method: "GET".to_owned(),
            url: url.to_owned(),
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"cz","type":"storefronts"}]}"#.to_vec(),
            stored_at: 0,
        };
        persist_json(storage.clone(), entry, uri(url)).await;
        assert!(
            storage
                .get_setting(STOREFRONT_SETTING)
                .await
                .unwrap()
                .unwrap()
                .contains("\"cz\"")
        );
    }
}
