//! Local playback: answers `WebPlayback` asset requests for tracks with a local
//! audio file and serves the local media URLs those answers point to.
//!
//! This only redirects playback to files the user already has on disk. It
//! never touches license acquisition, keys or encrypted Apple audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Limited};
use hudsucker::Body;
use hudsucker::hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};

use crate::apple;
use crate::catalog::{CatalogClient, CatalogItem, Song};
use crate::config::AutoCacheMode;
use crate::download_queue::{DownloadQueue, records_from_catalog};
use crate::interceptor_prelude::*;
use crate::media::{self, LocalAudio, MediaResolver, media_url};
use crate::respond;

/// Largest `WebPlayback` request body inspected.
const MAX_PLAYBACK_REQUEST: usize = 64 * 1024;

/// Request body fields that may carry the requested track id, in priority order.
const ID_FIELDS: &[&str] = &[
    "salableAdamId",
    "adamId",
    "universalLibraryId",
    "subscriptionAdamId",
    "id",
];

/// The identifying ids a `WebPlayback` request body carries for one track.
///
/// Auto-cache keys on the numeric catalog (adam) id, never the `i.…` library
/// id: the library id is not accepted by the catalog `songs/{id}` endpoint, so
/// feeding it there is what originally left library plays uncached.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "each field names the request body key it mirrors; the shared `_id` suffix is intentional"
)]
pub(crate) struct PlayIds {
    /// Numeric catalog (adam) id — the canonical download key, if the body
    /// carried a numeric id.
    pub(crate) catalog_id: Option<String>,
    /// `universalLibraryId` (`i.…`) of a library play, if present.
    pub(crate) library_id: Option<String>,
    /// Raw `subscriptionAdamId`, if present (the reliable catalog id on a
    /// library play).
    pub(crate) subscription_adam_id: Option<String>,
    /// Raw `salableAdamId`, if present.
    pub(crate) salable_adam_id: Option<String>,
}

/// Reads a body field as a non-empty string, turning a JSON number into its
/// digits.
fn id_field(body: &Value, field: &str) -> Option<String> {
    match body.get(field) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

impl PlayIds {
    /// Extracts the play ids from a `WebPlayback` JSON body. The catalog id is
    /// the first numeric id among `subscriptionAdamId`, `salableAdamId` and
    /// `adamId` — `subscriptionAdamId` first because it is the dependable
    /// catalog id on a library play.
    pub(crate) fn from_body(body: &Value) -> Self {
        let subscription_adam_id = id_field(body, "subscriptionAdamId");
        let salable_adam_id = id_field(body, "salableAdamId");
        let adam_id = id_field(body, "adamId");
        let catalog_id = [
            subscription_adam_id.clone(),
            salable_adam_id.clone(),
            adam_id,
        ]
        .into_iter()
        .flatten()
        .find(|s| s.bytes().all(|b| b.is_ascii_digit()));
        Self {
            catalog_id,
            library_id: id_field(body, "universalLibraryId"),
            subscription_adam_id,
            salable_adam_id,
        }
    }
}

/// Current unix time in milliseconds, saturating at `u64::MAX`.
#[must_use]
pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Background auto-cache component: enqueues a download for every track the
/// client asks to play, when the mode is not `Off`.
#[derive(Debug, Clone)]
pub(crate) struct AutoCache {
    mode: Arc<AtomicU8>,
    queue: DownloadQueue,
    catalog: CatalogClient,
    last_played_ts: Arc<AtomicU64>,
}

impl AutoCache {
    pub(crate) fn new(
        mode: Arc<AtomicU8>,
        queue: DownloadQueue,
        catalog: CatalogClient,
        last_played_ts: Arc<AtomicU64>,
    ) -> Self {
        Self {
            mode,
            queue,
            catalog,
            last_played_ts,
        }
    }

    fn current_mode(&self) -> AutoCacheMode {
        AutoCacheMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Fire-and-forget: refresh the playback timestamp and, unless auto-cache
    /// is off, enqueue the played track (deferred or immediate per mode).
    /// Errors are logged.
    fn schedule(&self, exchange: &Exchange, ids: PlayIds) {
        // Any playback event marks the user as active, independent of the
        // auto-cache mode — the queue uses this to decide when to drain
        // deferred rows.
        self.last_played_ts.store(now_unix_ms(), Ordering::Relaxed);

        let mode = self.current_mode();
        let deferred = match mode {
            AutoCacheMode::Off => return,
            AutoCacheMode::Instant => false,
            AutoCacheMode::Deferred => true,
        };
        let storage = exchange.storage().clone();
        let queue = self.queue.clone();
        let catalog = self.catalog.clone();
        exchange.spawn(async move {
            if let Err(error) = auto_cache_track(&storage, &queue, &catalog, &ids, deferred).await {
                tracing::warn!(?ids, %error, "auto-cache skipped");
            }
        });
    }
}

/// Resolves the numeric catalog (adam) id to download for a play: the one in
/// the body, or the catalog id a cached library row links to the library id.
/// Returns `None` (logging why) when neither is available.
async fn resolve_catalog_id(
    storage: &crate::storage::Storage,
    ids: &PlayIds,
) -> Result<Option<String>, String> {
    if let Some(catalog_id) = &ids.catalog_id {
        return Ok(Some(catalog_id.clone()));
    }
    let Some(library_id) = &ids.library_id else {
        tracing::debug!(?ids, "auto-cache: no catalog id to download");
        return Ok(None);
    };
    let catalog_id = storage
        .find_track(library_id)
        .await
        .map_err(|e| format!("find track: {e}"))?
        .and_then(|record| record.catalog_id);
    if catalog_id.is_none() {
        tracing::debug!(library = %library_id, "auto-cache: catalog id unknown");
    }
    Ok(catalog_id)
}

/// Runs the auto-cache workflow for one played track: resolves its catalog
/// (adam) id, checks the queue and local audio, gathers metadata (preferring
/// the cached track row over a network call), and enqueues the download keyed
/// on the catalog id.
async fn auto_cache_track(
    storage: &crate::storage::Storage,
    queue: &DownloadQueue,
    catalog: &CatalogClient,
    ids: &PlayIds,
    deferred: bool,
) -> Result<(), String> {
    // The download keys on the numeric catalog id; without one there is
    // nothing to fetch (reasons are logged while resolving it).
    let Some(catalog_id) = resolve_catalog_id(storage, ids).await? else {
        return Ok(());
    };

    // Skip if a download row already exists for this catalog id.
    let existing = storage
        .list_downloads()
        .await
        .map_err(|e| format!("list downloads: {e}"))?;
    if existing.iter().any(|row| row.track_id == catalog_id) {
        tracing::debug!(catalog = %catalog_id, "auto-cache: already queued");
        return Ok(());
    }

    // The cached track row (by catalog id, else by library id) supplies both
    // the "already have audio" check and, when present, the metadata — so a
    // library play Cider has already described needs no network call.
    let mut cached = storage
        .find_track(&catalog_id)
        .await
        .map_err(|e| format!("find track: {e}"))?;
    if cached.is_none()
        && let Some(library_id) = &ids.library_id
    {
        cached = storage
            .find_track(library_id)
            .await
            .map_err(|e| format!("find track: {e}"))?;
    }
    if let Some(record) = &cached
        && record.track.audio_path.is_some()
    {
        tracing::debug!(catalog = %catalog_id, "auto-cache: audio already present");
        return Ok(());
    }

    let song = if let Some(record) = cached {
        Song {
            id: catalog_id.clone(),
            title: record.track.title,
            artist: record.track.artist,
            album: record.track.album,
            duration_ms: record.track.duration_ms.unwrap_or(0),
            artwork_url: record.track.artwork_url,
        }
    } else {
        let mut song = catalog
            .get_song(&catalog_id)
            .await
            .map_err(|e| format!("get song: {e}"))?;
        // Key on the id we queried with, independent of the response.
        song.id.clone_from(&catalog_id);
        song
    };

    let mut records = records_from_catalog(&CatalogItem::Song(song));
    if records.is_empty() {
        return Ok(());
    }
    for record in &mut records {
        record.deferred = deferred;
    }
    queue
        .enqueue(records)
        .map_err(|e| format!("enqueue: {e}"))?;
    tracing::info!(catalog = %catalog_id, deferred, "auto-cache: enqueued");
    Ok(())
}

/// Serves local media and answers playback requests for local tracks.
#[derive(Debug, Clone)]
pub struct LocalPlaybackInterceptor {
    resolver: MediaResolver,
    auto_cache: Option<AutoCache>,
    queue: Option<DownloadQueue>,
}

impl LocalPlaybackInterceptor {
    pub(crate) fn new(resolver: MediaResolver) -> Self {
        Self {
            resolver,
            auto_cache: None,
            queue: None,
        }
    }

    pub(crate) fn with_auto_cache(mut self, auto_cache: AutoCache) -> Self {
        self.auto_cache = Some(auto_cache);
        self
    }

    /// Sets the queue used to prefetch a reference-audio custom song's missing
    /// catalog file. Independent of auto-cache: the prefetch fires even when
    /// auto-cache is off.
    pub(crate) fn with_queue(mut self, queue: DownloadQueue) -> Self {
        self.queue = Some(queue);
        self
    }

    /// If `id` is a custom song (`local-…`) that resolved to no local file,
    /// fire-and-forget a prefetch of its referenced catalog id (a no-op unless
    /// it is a reference-audio song whose file is missing and not yet queued).
    fn prefetch_reference(&self, exchange: &Exchange, id: &str) {
        let (Some(queue), true) = (
            self.queue.as_ref(),
            id.starts_with(media::CUSTOM_SONG_PREFIX),
        ) else {
            return;
        };
        let storage = exchange.storage().clone();
        let queue = queue.clone();
        let id = id.to_owned();
        exchange.spawn(async move {
            if let Err(error) =
                crate::download_queue::enqueue_missing_reference(&storage, &queue, &id).await
            {
                tracing::warn!(song = %id, %error, "reference prefetch failed");
            }
        });
    }
}

/// Extracts the requested track id from a `WebPlayback` JSON body.
pub(crate) fn requested_track_id(body: &Value) -> Option<String> {
    let as_id = |v: &Value| match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    ID_FIELDS
        .iter()
        .find_map(|field| body.get(*field).and_then(as_id))
        .or_else(|| body.pointer("/trackIds/0").and_then(as_id))
}

/// Builds a `WebPlayback`-shaped answer whose assets point at the local media URL.
pub(crate) fn playback_payload(song_id: &str, audio: &LocalAudio, file_size: u64) -> Value {
    let url = media_url(&audio.media_id);
    let extension = audio
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("m4a")
        .to_ascii_lowercase();
    let metadata = json!({
        "itemId": song_id,
        "itemName": audio.title.clone().unwrap_or_default(),
        "artistName": audio.artist.clone().unwrap_or_default(),
        "playlistName": audio.album.clone().unwrap_or_default(),
        "duration": audio.duration_ms.unwrap_or(0),
        "fileExtension": extension,
        "kind": "song",
        "bitRate": 256,
        "sampleRate": 44100,
    });
    let asset = |flavor: &str| {
        json!({
            "flavor": flavor,
            "URL": url,
            "file-size": file_size,
            "metadata": metadata,
        })
    };
    json!({
        "status": 0,
        "songList": [{
            "songId": song_id,
            "is-itunes-stream": true,
            "assets": [asset("28:ctrp256"), asset("2:256"), asset("10:128")],
        }],
    })
}

/// Apple-like failure answer for a track that cannot be played offline.
pub(crate) fn not_available_payload() -> Value {
    json!({
        "status": -1,
        "failureType": "3077",
        "customerMessage": "This song is not available offline.",
        "dialog": {
            "message": "This song is not available offline.",
            "explanation": "Import a local audio file for it in Cider Offline.",
            "okButtonString": "OK",
        },
    })
}

impl LocalPlaybackInterceptor {
    async fn serve_media(&self, exchange: &Exchange, media_id: Option<&str>) -> RequestOutcome {
        let headers = exchange.request_headers();
        if *exchange.method() == Method::OPTIONS {
            return RequestOutcome::Respond(respond::preflight(headers));
        }
        let audio = match media_id {
            Some(id) => self.resolver.resolve(id).await.unwrap_or_else(|error| {
                tracing::warn!(%error, "media lookup failed");
                None
            }),
            None => None,
        };
        if audio.is_none()
            && let Some(id) = media_id
        {
            self.prefetch_reference(exchange, id);
        }
        let response = match audio {
            Some(audio) => media::serve_file(&audio.path, exchange.method(), headers).await,
            None => respond::json(
                StatusCode::NOT_FOUND,
                &json!({"errors": [{"status": "404", "title": "Local media not found"}]}),
                headers,
                "media",
            ),
        };
        RequestOutcome::Respond(response)
    }

    async fn web_playback(&self, exchange: &Exchange, request: Request<Body>) -> RequestOutcome {
        let (parts, body) = request.into_parts();
        let bytes = match Limited::new(body, MAX_PLAYBACK_REQUEST).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                tracing::debug!(%error, "could not read playback request body");
                return RequestOutcome::Respond(respond::json(
                    StatusCode::BAD_REQUEST,
                    &json!({"status": -1}),
                    exchange.request_headers(),
                    "playback",
                ));
            }
        };
        let body = serde_json::from_slice::<Value>(&bytes).ok();
        let track_id = body.as_ref().and_then(requested_track_id);

        if let (Some(auto), Some(body)) = (self.auto_cache.as_ref(), body.as_ref()) {
            auto.schedule(exchange, PlayIds::from_body(body));
        }

        if let Some(track_id) = &track_id {
            match self.resolver.resolve(track_id).await {
                Ok(Some(audio)) => {
                    let size = tokio::fs::metadata(&audio.path)
                        .await
                        .map_or(0, |m| m.len());
                    tracing::info!(track = %track_id, "serving playback from local file");
                    return RequestOutcome::Respond(respond::json(
                        StatusCode::OK,
                        &playback_payload(track_id, &audio, size),
                        exchange.request_headers(),
                        "playback",
                    ));
                }
                Ok(None) => self.prefetch_reference(exchange, track_id),
                Err(error) => tracing::warn!(%error, "local audio lookup failed"),
            }
        }

        if !exchange.online() {
            return RequestOutcome::Respond(respond::json(
                StatusCode::OK,
                &not_available_payload(),
                exchange.request_headers(),
                "playback",
            ));
        }
        RequestOutcome::Continue(Request::from_parts(parts, Body::from(bytes)))
    }
}

#[async_trait]
impl Interceptor for LocalPlaybackInterceptor {
    fn name(&self) -> &'static str {
        "local-playback"
    }

    async fn on_request(&self, exchange: &mut Exchange, request: Request<Body>) -> RequestOutcome {
        if !apple::is_playback_host(exchange.host()) {
            return RequestOutcome::Continue(request);
        }
        let path = exchange.uri().path().to_owned();
        if path.starts_with(media::MEDIA_URL_PREFIX) {
            return self
                .serve_media(exchange, media::media_id_from_path(&path))
                .await;
        }
        if apple::is_web_playback_path(&path) && *exchange.method() == Method::POST {
            return self.web_playback(exchange, request).await;
        }
        RequestOutcome::Continue(request)
    }

    async fn on_upstream_error(&self, exchange: &mut Exchange) -> Option<Response<Body>> {
        (apple::is_playback_host(exchange.host())
            && apple::is_web_playback_path(exchange.uri().path()))
        .then(|| {
            respond::json(
                StatusCode::OK,
                &not_available_payload(),
                exchange.request_headers(),
                "playback",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn salable_adam_id_should_win() {
        assert_eq!(
            requested_track_id(&json!({"salableAdamId": "1", "universalLibraryId": "i.2"}))
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn numeric_id_should_be_accepted() {
        assert_eq!(
            requested_track_id(&json!({"adamId": 15})).as_deref(),
            Some("15")
        );
    }

    #[test]
    fn library_id_should_be_used_without_adam_id() {
        assert_eq!(
            requested_track_id(&json!({"universalLibraryId": "i.2"})).as_deref(),
            Some("i.2")
        );
    }

    #[test]
    fn play_ids_should_take_catalog_id_from_subscription_on_library_play() {
        let ids = PlayIds::from_body(&json!({
            "universalLibraryId": "i.FakeLibSong0003",
            "subscriptionAdamId": "1484636829",
        }));
        assert_eq!(ids.catalog_id.as_deref(), Some("1484636829"));
        assert_eq!(ids.library_id.as_deref(), Some("i.FakeLibSong0003"));
        assert_eq!(ids.subscription_adam_id.as_deref(), Some("1484636829"));
    }

    #[test]
    fn play_ids_should_ignore_library_id_as_catalog_id() {
        // `universalLibraryId` is not numeric, so it is never the catalog id.
        let ids = PlayIds::from_body(&json!({"universalLibraryId": "i.2"}));
        assert_eq!(ids.catalog_id, None);
        assert_eq!(ids.library_id.as_deref(), Some("i.2"));
    }

    #[test]
    fn play_ids_should_accept_salable_id_for_catalog_play() {
        let ids = PlayIds::from_body(&json!({"salableAdamId": 15}));
        assert_eq!(ids.catalog_id.as_deref(), Some("15"));
        assert_eq!(ids.salable_adam_id.as_deref(), Some("15"));
    }

    #[test]
    fn payload_should_point_to_local_media_url() {
        let audio = LocalAudio {
            media_id: "1".to_owned(),
            path: PathBuf::from("x.mp3"),
            duration_ms: Some(1000),
            title: None,
            artist: None,
            album: None,
        };
        let payload = playback_payload("1", &audio, 10);
        assert_eq!(
            payload
                .pointer("/songList/0/assets/0/URL")
                .and_then(Value::as_str),
            Some("https://play.itunes.apple.com/__cider-offline/media/1")
        );
    }
}
