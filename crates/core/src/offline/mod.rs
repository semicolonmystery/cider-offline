//! Offline responder: answers Apple requests locally when Apple Music is
//! unreachable (the connectivity monitor says offline, the proxy is forced
//! offline, or forwarding a request failed).
//!
//! Order of answers:
//!
//! 1. artwork (`*.mzstatic.com`, `*.blobstore.apple.com`): the best stored
//!    variant of the requested image, any size or format;
//! 2. the JSON API endpoints the local library can answer are **synthesized**
//!    from the API-synced library and stored track metadata ([`api`]), with
//!    Cider's query parameters honoured (`include`, `fields`, `extend`,
//!    `limit` / `offset` + `next`, `ids[...]`, `format[resources]=map`);
//! 3. anything else replays an exactly matching cached response, if any;
//! 4. otherwise a harmless empty document, so pages keep rendering.
//!
//! Songs without local audio keep their place in every list but lose their
//! `playParams`, so Cider shows them as unavailable and skips them when it
//! builds a queue, instead of failing playback mid-queue.
//!
//! The library Songs page follows [`SongsPage`] online too: with
//! [`SongsPage::All`] its listing (`/v1/me/library/songs` without `ids`) is
//! answered from the index even while Apple is reachable, because Apple
//! itself only lists the Songs tab. With [`SongsPage::Apple`] it goes to
//! Apple while online (Apple's own, always current answer) and is answered
//! from the synced Songs tab while offline.

mod api;
mod index;
mod query;
mod render;
mod search;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::Bytes;
use hudsucker::Body;
use hudsucker::hyper::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue};
use hudsucker::hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use self::api::Answer;
use self::index::LocalIndex;
use self::query::Query;
use crate::apple;
use crate::artwork::ArtworkStore;
use crate::config::SongsPage;
use crate::inject;
use crate::interceptor_prelude::*;
use crate::playback::not_available_payload;
use crate::respond::{self, LOCAL_HEADER, add_cors};
use crate::storage::{CacheEntry, Storage, StorageError};

/// `Cache-Control` of artwork served offline (images are immutable).
const ARTWORK_CACHE_CONTROL: &str = "public, max-age=86400";

/// The index together with the store fingerprint it was built from.
#[derive(Debug)]
struct CachedIndex {
    fingerprint: String,
    index: Arc<LocalIndex>,
}

/// Offline responder (see the [module docs](self)).
#[derive(Debug, Clone)]
pub struct OfflineResponder {
    artwork: ArtworkStore,
    index: Arc<Mutex<Option<CachedIndex>>>,
    /// Live [`SongsPage`] setting ([`SongsPage::as_u8`]).
    songs_page: Arc<AtomicU8>,
}

impl OfflineResponder {
    /// `songs_page` is read on every request, so changing it applies at once.
    pub(crate) fn new(artwork: ArtworkStore, songs_page: Arc<AtomicU8>) -> Self {
        Self {
            artwork,
            index: Arc::new(Mutex::new(None)),
            songs_page,
        }
    }

    fn songs_page(&self) -> SongsPage {
        SongsPage::from_u8(self.songs_page.load(Ordering::Relaxed))
    }

    /// Whether an online request is answered locally anyway: the Songs page
    /// listing when every library song is to be listed.
    fn answers_online(&self, exchange: &Exchange) -> bool {
        self.songs_page() == SongsPage::All
            && matches!(*exchange.method(), Method::GET | Method::HEAD)
            && apple::is_api_host(exchange.host())
            && api::is_songs_listing(exchange.uri().path(), &Query::parse(exchange.uri()))
    }

    /// The current index, rebuilt when the store changed since the last build.
    async fn index(&self, storage: &Storage) -> Result<Arc<LocalIndex>, StorageError> {
        let fingerprint = storage.offline_fingerprint().await?;
        let mut cached = self.index.lock().await;
        if let Some(current) = cached.as_ref()
            && current.fingerprint == fingerprint
        {
            return Ok(Arc::clone(&current.index));
        }
        let index = Arc::new(LocalIndex::load(storage).await?);
        tracing::debug!(
            collections = index.collections.len(),
            tracks = index.tracks.len(),
            "offline index rebuilt"
        );
        *cached = Some(CachedIndex {
            fingerprint,
            index: Arc::clone(&index),
        });
        Ok(index)
    }

    async fn answer(&self, exchange: &Exchange) -> Response<Body> {
        let headers = exchange.request_headers();
        if *exchange.method() == Method::OPTIONS {
            return respond::preflight(headers);
        }
        let host = exchange.host();
        let path = exchange.uri().path();
        let is_read = matches!(*exchange.method(), Method::GET | Method::HEAD);

        if is_read && (apple::is_artwork_host(host) || apple::is_blobstore_host(host)) {
            return self.artwork_response(exchange).await;
        }
        if apple::is_playback_host(host) && apple::is_web_playback_path(path) {
            return respond::json(StatusCode::OK, &not_available_payload(), headers, "offline");
        }
        if apple::is_activity_host(host) {
            // Play-activity reports cannot be delivered offline; accept them.
            return respond::json(StatusCode::OK, &json!({}), headers, "offline");
        }
        if apple::is_api_host(host) {
            match self.api_answer(exchange).await {
                Ok(Some(response)) => return response,
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "offline synthesis failed"),
            }
            if is_read && let Some(response) = self.cached(exchange).await {
                return response;
            }
            if is_read {
                let mut document = json!({"data": []});
                inject_offline(exchange, &mut document).await;
                return respond::json(StatusCode::OK, &document, headers, "offline");
            }
        }
        respond::json(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"errors": [{"status": "503", "title": "Offline"}]}),
            headers,
            "offline",
        )
    }

    /// A synthesized API answer, or `None` when the index does not own the
    /// endpoint.
    async fn api_answer(
        &self,
        exchange: &Exchange,
    ) -> Result<Option<Response<Body>>, StorageError> {
        let index = self.index(exchange.storage()).await?;
        let query = Query::parse(exchange.uri());
        let answer = api::answer(
            &index,
            exchange.method(),
            exchange.uri().path(),
            &query,
            self.songs_page(),
        );
        let headers = exchange.request_headers();
        let status = answer.status();
        Ok(match answer {
            Answer::Json(mut document) => {
                inject_offline(exchange, &mut document).await;
                Some(respond::json(status, &document, headers, "offline"))
            }
            Answer::NotFound => {
                // A custom item may still match (`local-…` ids).
                let mut document = json!({"data": []});
                if inject::inject_into(
                    exchange.storage(),
                    exchange.method(),
                    exchange.uri(),
                    &mut document,
                )
                .await
                .unwrap_or(false)
                    && document
                        .get("data")
                        .and_then(Value::as_array)
                        .is_some_and(|d| !d.is_empty())
                {
                    Some(respond::json(StatusCode::OK, &document, headers, "offline"))
                } else {
                    Some(respond::json(
                        status,
                        &api::not_found_document(),
                        headers,
                        "offline",
                    ))
                }
            }
            Answer::Unowned => None,
        })
    }

    /// Replays an exactly matching cached response.
    async fn cached(&self, exchange: &Exchange) -> Option<Response<Body>> {
        if apple::is_sensitive_path(exchange.uri().path()) {
            return None;
        }
        let key = exchange.cache_key()?;
        match exchange.storage().get_cache_entry(&key).await {
            Ok(Some(entry)) => Some(cached_answer(exchange, entry).await),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "cache lookup failed");
                None
            }
        }
    }

    async fn artwork_response(&self, exchange: &Exchange) -> Response<Body> {
        let headers = exchange.request_headers();
        let edge = crate::artwork::requested_edge(exchange.uri());
        let stored = self
            .artwork
            .lookup_sized(exchange.uri(), edge)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "artwork lookup failed");
                None
            });
        let Some(stored) = stored else {
            return not_found_image(headers);
        };
        match tokio::fs::read(&stored.path).await {
            Ok(bytes) => {
                let body = if *exchange.method() == Method::HEAD {
                    Bytes::new()
                } else {
                    Bytes::from(bytes)
                };
                let mut response = respond::bytes_response(
                    StatusCode::OK,
                    &stored.content_type,
                    body,
                    headers,
                    "artwork",
                );
                response.headers_mut().insert(
                    CACHE_CONTROL,
                    HeaderValue::from_static(ARTWORK_CACHE_CONTROL),
                );
                response
            }
            Err(error) => {
                tracing::warn!(%error, "failed to read artwork file");
                not_found_image(headers)
            }
        }
    }
}

fn not_found_image(headers: &hudsucker::hyper::HeaderMap) -> Response<Body> {
    let mut response = respond::bytes_response(
        StatusCode::NOT_FOUND,
        "text/plain",
        Bytes::new(),
        headers,
        "offline",
    );
    response.headers_mut().remove(CONTENT_TYPE);
    response
}

/// Rebuilds a stored response.
pub(crate) fn cached_response(
    entry: CacheEntry,
    request_headers: &hudsucker::hyper::HeaderMap,
) -> Response<Body> {
    let mut response = Response::new(Body::from(Bytes::from(entry.body)));
    *response.status_mut() = StatusCode::from_u16(entry.status).unwrap_or(StatusCode::OK);
    let headers = response.headers_mut();
    for (name, value) in &entry.headers {
        if !respond::is_replayable_header(name)
            || name.to_ascii_lowercase().starts_with("access-control-")
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    add_cors(headers, request_headers);
    headers.insert(LOCAL_HEADER, HeaderValue::from_static("cache"));
    response
}

/// Merges custom items into an offline document, logging failures. This is the
/// offline half of the shared injection path (the online half is
/// [`crate::inject::LibraryInjectionInterceptor`]).
async fn inject_offline(exchange: &Exchange, document: &mut Value) {
    if let Err(error) = inject::inject_into(
        exchange.storage(),
        exchange.method(),
        exchange.uri(),
        document,
    )
    .await
    {
        tracing::warn!(%error, "offline library injection failed");
    }
}

/// Answers from a cache hit, merging custom items into a cached library / search
/// document so fake items surface offline exactly as they do online.
async fn cached_answer(exchange: &Exchange, entry: CacheEntry) -> Response<Body> {
    let headers = exchange.request_headers();
    if *exchange.method() == Method::GET
        && let Ok(mut document) = serde_json::from_slice::<Value>(&entry.body)
    {
        match inject::inject_into(
            exchange.storage(),
            exchange.method(),
            exchange.uri(),
            &mut document,
        )
        .await
        {
            Ok(true) => {
                let entry = CacheEntry {
                    body: document.to_string().into_bytes(),
                    ..entry
                };
                return cached_response(entry, headers);
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "offline library injection failed"),
        }
    }
    cached_response(entry, headers)
}

#[async_trait]
impl Interceptor for OfflineResponder {
    fn name(&self) -> &'static str {
        "offline-responder"
    }

    async fn on_request(&self, exchange: &mut Exchange, request: Request<Body>) -> RequestOutcome {
        if exchange.online() && !self.answers_online(exchange) {
            return RequestOutcome::Continue(request);
        }
        RequestOutcome::Respond(self.answer(exchange).await)
    }

    async fn on_upstream_error(&self, exchange: &mut Exchange) -> Option<Response<Body>> {
        Some(self.answer(exchange).await)
    }
}
