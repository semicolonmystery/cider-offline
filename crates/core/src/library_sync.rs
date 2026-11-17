//! Active sync of the user's Apple Music library from the Apple Music API.
//!
//! Learning the library only *passively*, from whatever JSON Cider happens
//! to request through the proxy, gives partial first pages, nested resources
//! from `include=` relationships and stale playlists. This module fetches the
//! library itself with the captured tokens instead:
//!
//! 1. every library playlist (`GET /v1/me/library/playlists`, all pages);
//! 2. every library album (`GET /v1/me/library/albums`, all pages);
//! 3. for each of them, the complete ordered track list
//!    (`GET /v1/me/library/{playlists|albums}/{id}/tracks?include=catalog`,
//!    every page, checked against Apple's `meta.total`);
//! 4. the library Songs tab (`GET /v1/me/library/songs?include=catalog&sort=dateAdded`,
//!    all pages): only the songs the user added themselves, in the order
//!    Cider's Songs page requests. Songs that are merely in a library
//!    playlist are not part of it.
//!
//! The result is applied with [`Storage::replace_library`] as ONE transaction,
//! and only when every request succeeded — a failed or partial sync never
//! touches the stored library. The Apple Music library view reads nothing
//! else.
//!
//! Pagination: Apple pages these endpoints with `offset` / `limit` (at most
//! `100`, larger values are rejected with HTTP 400) and returns a relative
//! `next` link while more remain. The `next` link drops `limit` / `include`,
//! so each page is requested with explicit parameters and only the `offset`
//! is taken from `next`. An empty playlist's `/tracks` answers HTTP 404 with
//! error code `40403` ("No related resources"), which is an empty list.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::artwork::{LARGE_EDGE, SMALL_EDGE};
use crate::cache::STOREFRONT_SETTING;
use crate::catalog::{collection_from_resource, playable_from_resource};
use crate::library::{ArtworkRequest, Library, PrefetchReport};
use crate::proxy::ProxyStatus;
use crate::storage::{
    CollectionKind, LibrarySnapshot, Playlist, Storage, StorageError, SyncedCollection, TokenKind,
    TrackMetadata,
};

const API_BASE: &str = "https://amp-api.music.apple.com";
const ORIGIN_VALUE: &str = "https://music.apple.com";
const REFERER_VALUE: &str = "https://music.apple.com/";
/// The desktop Chrome user agent Cider's own requests carry.
const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";
/// Largest page size Apple accepts on the library endpoints.
const PAGE_LIMIT: u32 = 100;
/// Guard against a malformed `next` chain.
const MAX_PAGES: usize = 1_000;
/// Concurrent track-list fetches.
const CONCURRENCY: usize = 4;
/// Retries for rate limiting (429) / transient server errors (5xx).
const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Concurrent cover downloads of the post-sync artwork prefetch.
pub const PREFETCH_CONCURRENCY: usize = 6;
/// Periodic re-sync interval.
pub const SYNC_INTERVAL: Duration = Duration::from_mins(30);
/// A token change within this long after a successful sync does not trigger
/// another one (tokens are re-captured on every app start).
const TOKEN_RESYNC_DEBOUNCE: Duration = Duration::from_mins(2);

/// Errors of one sync run. Nothing is stored when a run fails.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// A required token has not been captured yet.
    #[error("no {0} captured yet; open Cider through the proxy once")]
    MissingToken(&'static str),
    /// A stored token is not a valid header value.
    #[error("a captured token is not a valid header value")]
    InvalidToken,
    /// The HTTP request failed.
    #[error("Apple Music request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Apple answered with an error status.
    #[error("Apple Music returned HTTP {status} for {path}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Request path.
        path: String,
        /// Start of the error body.
        body: String,
    },
    /// The response did not have the expected shape.
    #[error("unexpected Apple Music response for {path}: {reason}")]
    Invalid {
        /// Request path.
        path: String,
        /// What was wrong.
        reason: String,
    },
    /// Fewer / more items arrived across all pages than Apple reported.
    #[error("incomplete listing for {path}: got {got} items, Apple reported {expected}")]
    Incomplete {
        /// Request path.
        path: String,
        /// Items received across all pages.
        got: usize,
        /// `meta.total` Apple reported.
        expected: u64,
    },
    /// Database error.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A fetch task panicked.
    #[error("sync task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Observable state of the library sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStatus {
    /// A sync is in progress.
    pub running: bool,
    /// Number of successful syncs since start (changes when the stored
    /// library was replaced).
    pub completed: u64,
    /// Unix seconds of the last successful sync.
    pub last_success_unix: Option<u64>,
    /// Error of the last run, `None` after a success.
    pub last_error: Option<String>,
    /// Playlists in the last successful snapshot.
    pub playlists: usize,
    /// Albums in the last successful snapshot.
    pub albums: usize,
    /// Songs of the library Songs tab in the last successful snapshot.
    pub songs: usize,
}

/// All items of a paginated listing.
#[derive(Debug, Default)]
struct Listing {
    items: Vec<Value>,
    reported_total: Option<u64>,
}

/// Minimal client for the library endpoints.
#[derive(Debug, Clone)]
struct AppleApi {
    http: reqwest::Client,
}

impl AppleApi {
    fn new() -> Result<Self, SyncError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { http })
    }

    /// Cider's request headers with the captured tokens (marked sensitive so
    /// they never appear in debug output).
    async fn headers(storage: &Storage) -> Result<HeaderMap, SyncError> {
        let developer = storage
            .get_auth_token(TokenKind::DeveloperToken)
            .await?
            .filter(|t| !t.trim().is_empty())
            .ok_or(SyncError::MissingToken("developer token"))?;
        let media_user = storage
            .get_auth_token(TokenKind::MediaUserToken)
            .await?
            .filter(|t| !t.trim().is_empty())
            .ok_or(SyncError::MissingToken("media-user token"))?;
        // The captured `Authorization` value normally already is `Bearer …`.
        let developer = if developer
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
        {
            developer
        } else {
            format!("Bearer {developer}")
        };
        let mut authorization =
            HeaderValue::from_str(&developer).map_err(|_| SyncError::InvalidToken)?;
        authorization.set_sensitive(true);
        let mut user = HeaderValue::from_str(&media_user).map_err(|_| SyncError::InvalidToken)?;
        user.set_sensitive(true);

        let mut headers = HeaderMap::with_capacity(6);
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(HeaderName::from_static("media-user-token"), user);
        headers.insert(ORIGIN, HeaderValue::from_static(ORIGIN_VALUE));
        headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    /// One page; `None` for Apple's "no related resources" 404 (an empty list).
    async fn page(
        &self,
        headers: &HeaderMap,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Option<Value>, SyncError> {
        let url = format!("{API_BASE}{path}");
        let mut attempt = 0;
        loop {
            let response = self
                .http
                .get(&url)
                .headers(headers.clone())
                .query(query)
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(Some(response.json::<Value>().await?));
            }
            let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if retryable && attempt < MAX_RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                continue;
            }
            let body = response.text().await.unwrap_or_default();
            if status == StatusCode::NOT_FOUND && is_no_related_resources(&body) {
                return Ok(None);
            }
            return Err(SyncError::Status {
                status: status.as_u16(),
                path: path.to_owned(),
                body: body.chars().take(300).collect(),
            });
        }
    }

    /// Every item of a paginated listing, verified against `meta.total`.
    /// `extra` query parameters are sent with every page.
    async fn listing(
        &self,
        headers: &HeaderMap,
        path: &str,
        extra: &[(&str, &str)],
    ) -> Result<Listing, SyncError> {
        let mut listing = Listing::default();
        let mut offset: u64 = 0;
        for _ in 0..MAX_PAGES {
            let mut query = vec![
                ("offset", offset.to_string()),
                ("limit", PAGE_LIMIT.to_string()),
            ];
            query.extend(extra.iter().map(|&(name, value)| (name, value.to_owned())));
            let Some(page) = self.page(headers, path, &query).await? else {
                if offset == 0 {
                    listing.reported_total = Some(0);
                    return Ok(listing);
                }
                return Err(invalid(path, "a later page answered 404"));
            };
            let data = page
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid(path, "no data array"))?;
            listing.items.extend(data.iter().cloned());
            if let Some(total) = page.pointer("/meta/total").and_then(Value::as_u64) {
                listing.reported_total = Some(total);
            }
            let Some(next) = page.get("next").and_then(Value::as_str) else {
                if let Some(expected) = listing.reported_total
                    && usize::try_from(expected).ok() != Some(listing.items.len())
                {
                    return Err(SyncError::Incomplete {
                        path: path.to_owned(),
                        got: listing.items.len(),
                        expected,
                    });
                }
                return Ok(listing);
            };
            let next_offset =
                next_offset(next).ok_or_else(|| invalid(path, "next link without offset"))?;
            if next_offset <= offset {
                return Err(invalid(path, "next link does not advance"));
            }
            offset = next_offset;
        }
        Err(invalid(path, "too many pages"))
    }
}

fn invalid(path: &str, reason: &str) -> SyncError {
    SyncError::Invalid {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Whether an error body is Apple's `40403` "No related resources".
fn is_no_related_resources(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|value| {
        value
            .pointer("/errors/0/code")
            .and_then(Value::as_str)
            .is_some_and(|code| code == "40403")
    })
}

/// The `offset` query parameter of a (relative) `next` link.
fn next_offset(next: &str) -> Option<u64> {
    let url = reqwest::Url::parse(API_BASE).ok()?.join(next).ok()?;
    url.query_pairs()
        .find(|(name, _)| name == "offset")
        .and_then(|(_, value)| value.parse().ok())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Query of the Songs tab listing: Cider's Songs page order, with the
/// catalog twins so every song is playable and searchable offline.
const SONGS_QUERY: &[(&str, &str)] = &[("include", "catalog"), ("sort", "dateAdded")];
/// Query of a collection's track list.
const TRACKS_QUERY: &[(&str, &str)] = &[("include", "catalog")];
const SONGS_PATH: &str = "/v1/me/library/songs";

/// Builds the snapshot from the fetched listings (pure; unit-tested).
fn build_snapshot(
    collections: Vec<(Playlist, Listing)>,
    songs: &Listing,
) -> Result<LibrarySnapshot, SyncError> {
    let mut snapshot = LibrarySnapshot::default();
    let mut seen_tracks: HashSet<String> = HashSet::new();
    for item in &songs.items {
        let track: TrackMetadata = playable_from_resource(item)
            .ok_or_else(|| invalid(SONGS_PATH, "unparsable library song"))?;
        snapshot.songs.push(track.id.clone());
        if seen_tracks.insert(track.id.clone()) {
            snapshot.tracks.push(track);
        }
    }
    for (collection, listing) in collections {
        let path = format!("{} tracks", collection.id);
        let mut track_ids = Vec::with_capacity(listing.items.len());
        for item in &listing.items {
            let track: TrackMetadata = playable_from_resource(item)
                .ok_or_else(|| invalid(&path, "unparsable track entry"))?;
            track_ids.push(track.id.clone());
            if seen_tracks.insert(track.id.clone()) {
                snapshot.tracks.push(track);
            }
        }
        snapshot.collections.push(SyncedCollection {
            collection,
            track_ids,
            reported_total: listing
                .reported_total
                .and_then(|total| u32::try_from(total).ok()),
        });
    }
    Ok(snapshot)
}

struct Inner {
    storage: Storage,
    api: AppleApi,
    /// Explicit sync requests (UI Refresh).
    requested: Notify,
    /// Auth tokens were captured / changed.
    tokens_changed: Notify,
    status: watch::Sender<SyncStatus>,
    /// Serializes runs (scheduled vs. requested vs. headless).
    gate: Mutex<()>,
    /// Downloads the covers of a synced library to the artwork cache.
    prefetch: Option<Library>,
}

/// Handle to the library sync. Cheap to clone.
#[derive(Clone)]
pub struct LibrarySync {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LibrarySync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibrarySync")
            .field("status", &*self.inner.status.borrow())
            .finish_non_exhaustive()
    }
}

impl LibrarySync {
    /// Creates the sync over `storage` (reads tokens, writes the library).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(storage: Storage) -> Result<Self, SyncError> {
        Self::build(storage, None)
    }

    /// Like [`Self::new`], and after every successful background sync also
    /// downloads every collection cover and every distinct track cover of the
    /// synced library into `library`'s artwork cache (see
    /// [`Self::prefetch_artwork`]), so opening a playlist needs no network.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn with_artwork_prefetch(storage: Storage, library: Library) -> Result<Self, SyncError> {
        Self::build(storage, Some(library))
    }

    fn build(storage: Storage, prefetch: Option<Library>) -> Result<Self, SyncError> {
        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                api: AppleApi::new()?,
                requested: Notify::new(),
                tokens_changed: Notify::new(),
                status: watch::Sender::new(SyncStatus::default()),
                gate: Mutex::new(()),
                prefetch,
            }),
        })
    }

    /// Asks the background loop to sync as soon as possible (UI Refresh).
    pub fn request_sync(&self) {
        self.inner.requested.notify_one();
    }

    /// Signals that auth tokens were captured or changed.
    pub(crate) fn tokens_changed(&self) {
        self.inner.tokens_changed.notify_one();
    }

    /// Subscribes to sync status updates.
    pub fn subscribe(&self) -> watch::Receiver<SyncStatus> {
        self.inner.status.subscribe()
    }

    /// Current status snapshot.
    pub fn status(&self) -> SyncStatus {
        self.inner.status.borrow().clone()
    }

    /// Runs one complete sync now and stores it; returns the snapshot that
    /// was stored. Concurrent calls run one after another.
    ///
    /// # Errors
    ///
    /// Any request / validation / database error; the stored library is then
    /// left unchanged.
    pub async fn sync_now(&self) -> Result<LibrarySnapshot, SyncError> {
        let _gate = self.inner.gate.lock().await;
        self.inner.status.send_modify(|s| s.running = true);
        let result = self.fetch_and_store().await;
        self.inner.status.send_modify(|s| {
            s.running = false;
            match &result {
                Ok(snapshot) => {
                    s.completed += 1;
                    s.last_success_unix = Some(unix_now());
                    s.last_error = None;
                    s.playlists = count_kind(snapshot, CollectionKind::Playlist);
                    s.albums = count_kind(snapshot, CollectionKind::Album);
                    s.songs = snapshot.songs.len();
                }
                Err(error) => s.last_error = Some(error.to_string()),
            }
        });
        result
    }

    /// The covers a synced library shows: every collection cover at the grid
    /// size and every track cover at the row size. Tracks of one album share
    /// one cover identity, so [`Library::prefetch_artwork`] fetches it once.
    pub fn artwork_requests(snapshot: &LibrarySnapshot) -> Vec<ArtworkRequest> {
        let collections = snapshot
            .collections
            .iter()
            .filter_map(|synced| synced.collection.artwork_url.clone())
            .map(|url| ArtworkRequest {
                url,
                edge: LARGE_EDGE,
            });
        let tracks = snapshot
            .tracks
            .iter()
            .filter_map(|track| track.artwork_url.clone())
            .map(|url| ArtworkRequest {
                url,
                edge: SMALL_EDGE,
            });
        collections.chain(tracks).collect()
    }

    /// Downloads the covers of `snapshot` that are not cached yet (bounded
    /// concurrency, cancellation-safe: completed files stay, nothing else is
    /// written). `None` when this sync has no artwork prefetch configured.
    pub async fn prefetch_artwork(
        &self,
        snapshot: &LibrarySnapshot,
        cancel: CancellationToken,
    ) -> Option<PrefetchReport> {
        let library = self.inner.prefetch.as_ref()?;
        let requests = Self::artwork_requests(snapshot);
        Some(
            library
                .prefetch_artwork(requests, PREFETCH_CONCURRENCY, cancel)
                .await,
        )
    }

    /// Starts [`Self::prefetch_artwork`] in the background, cancelling the
    /// previous run (its token is in `current`).
    fn spawn_prefetch(
        &self,
        snapshot: LibrarySnapshot,
        parent: &CancellationToken,
        current: &mut Option<CancellationToken>,
    ) {
        if self.inner.prefetch.is_none() {
            return;
        }
        if let Some(previous) = current.take() {
            previous.cancel();
        }
        let token = parent.child_token();
        *current = Some(token.clone());
        let sync = self.clone();
        tokio::spawn(async move {
            if let Some(report) = sync.prefetch_artwork(&snapshot, token).await {
                tracing::info!(
                    requested = report.requested,
                    cached = report.cached,
                    fetched = report.fetched,
                    failed = report.failed,
                    cancelled = report.cancelled,
                    "library artwork prefetched"
                );
            }
        });
    }

    async fn fetch_and_store(&self) -> Result<LibrarySnapshot, SyncError> {
        let snapshot = self.fetch().await?;
        self.inner.storage.replace_library(snapshot.clone()).await?;
        self.store_storefront().await;
        if let Some(library) = &self.inner.prefetch {
            library.link_existing_media().await;
        }
        Ok(snapshot)
    }

    /// Saves the account's storefront document (`/v1/me/storefront`) so the
    /// offline responder can answer it even if Cider's own request was never
    /// seen. Best effort: a failure only logs.
    async fn store_storefront(&self) {
        let storage = &self.inner.storage;
        let result = async {
            let headers = AppleApi::headers(storage).await?;
            self.inner
                .api
                .page(&headers, "/v1/me/storefront", &[])
                .await
        }
        .await;
        match result {
            Ok(Some(document)) if document.pointer("/data/0/id").is_some() => {
                if let Err(error) = storage
                    .set_setting(STOREFRONT_SETTING, document.to_string())
                    .await
                {
                    tracing::warn!(%error, "failed to save the storefront");
                }
            }
            Ok(_) => tracing::debug!("storefront response without data"),
            Err(error) => tracing::debug!(%error, "storefront fetch failed"),
        }
    }

    /// Fetches the complete library (no storage writes).
    async fn fetch(&self) -> Result<LibrarySnapshot, SyncError> {
        let storage = &self.inner.storage;
        let api = &self.inner.api;
        let headers = Arc::new(AppleApi::headers(storage).await?);

        let mut collections = Vec::new();
        for (path, kind) in [
            ("/v1/me/library/playlists", "playlists"),
            ("/v1/me/library/albums", "albums"),
        ] {
            let listing = api.listing(&headers, path, &[]).await?;
            for item in &listing.items {
                let collection = collection_from_resource(item)
                    .ok_or_else(|| invalid(path, "unparsable library collection"))?;
                collections.push((kind, collection));
            }
        }

        // Track lists, a few at a time. Dropping the JoinSet (on error or
        // cancellation) aborts the remaining fetches.
        let limit = Arc::new(Semaphore::new(CONCURRENCY));
        let mut tasks = JoinSet::new();
        for (index, (kind, collection)) in collections.iter().enumerate() {
            let path = format!("/v1/me/library/{kind}/{}/tracks", collection.id);
            let api = api.clone();
            let headers = Arc::clone(&headers);
            let limit = Arc::clone(&limit);
            tasks.spawn(async move {
                let _permit = limit.acquire_owned().await;
                api.listing(&headers, &path, TRACKS_QUERY)
                    .await
                    .map(|listing| (index, listing))
            });
        }
        let songs = api.listing(&headers, SONGS_PATH, SONGS_QUERY).await?;
        let mut listings: HashMap<usize, Listing> = HashMap::with_capacity(collections.len());
        while let Some(joined) = tasks.join_next().await {
            let (index, listing) = joined??;
            listings.insert(index, listing);
        }
        let ordered = collections
            .into_iter()
            .enumerate()
            .map(|(index, (_, collection))| {
                let listing = listings.remove(&index).unwrap_or_default();
                (collection, listing)
            })
            .collect();
        build_snapshot(ordered, &songs)
    }

    /// Background loop: syncs on start, every [`SYNC_INTERVAL`], on request,
    /// when tokens change and when connectivity returns; skips while offline.
    /// Ends when `cancel` fires (an in-flight sync is dropped, which is safe:
    /// storage is only written in the final single transaction).
    pub(crate) async fn run(
        self,
        cancel: CancellationToken,
        mut proxy_status: watch::Receiver<ProxyStatus>,
    ) {
        let mut interval = tokio::time::interval(SYNC_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut was_online = proxy_status.borrow().online;
        let mut prefetch: Option<CancellationToken> = None;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {}
                () = self.inner.requested.notified() => {}
                () = self.inner.tokens_changed.notified() => {
                    let recent = self.status().last_success_unix.is_some_and(|at| {
                        unix_now().saturating_sub(at) < TOKEN_RESYNC_DEBOUNCE.as_secs()
                    });
                    if recent {
                        continue;
                    }
                }
                changed = proxy_status.changed() => {
                    if changed.is_err() {
                        break; // the proxy is gone
                    }
                    let online = proxy_status.borrow_and_update().online;
                    let back_online = online && !was_online;
                    was_online = online;
                    if !back_online {
                        continue;
                    }
                }
            }
            if !proxy_status.borrow().online {
                tracing::debug!("offline; library sync skipped");
                continue;
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                result = self.sync_now() => match result {
                    Ok(snapshot) => {
                        tracing::info!(
                            collections = snapshot.collections.len(),
                            tracks = snapshot.tracks.len(),
                            songs = snapshot.songs.len(),
                            "library synced from Apple Music"
                        );
                        self.spawn_prefetch(snapshot, &cancel, &mut prefetch);
                    }
                    Err(SyncError::MissingToken(token)) => {
                        tracing::info!(token, "library sync waiting for tokens");
                    }
                    Err(error) => tracing::warn!(%error, "library sync failed"),
                },
            }
        }
    }
}

fn count_kind(snapshot: &LibrarySnapshot, kind: CollectionKind) -> usize {
    snapshot
        .collections
        .iter()
        .filter(|c| c.collection.kind == kind)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn song(id: &str, name: &str) -> Value {
        json!({"id": id, "type": "library-songs", "attributes": {
            "name": name, "artistName": "A", "albumName": "Al",
            "playParams": {"catalogId": format!("9{id}"), "id": id, "kind": "song"}}})
    }

    #[test]
    fn artwork_requests_should_size_collections_large_and_tracks_small() {
        let snapshot = LibrarySnapshot {
            collections: vec![SyncedCollection {
                collection: Playlist {
                    artwork_url: Some("https://is1-ssl.mzstatic.com/c/{w}x{h}bb.jpg".to_owned()),
                    ..Playlist::minimal("p.x", CollectionKind::Playlist, "X")
                },
                track_ids: vec!["i.1".to_owned()],
                reported_total: Some(1),
            }],
            tracks: vec![TrackMetadata {
                id: "i.1".to_owned(),
                artwork_url: Some("https://is1-ssl.mzstatic.com/t/{w}x{h}bb.jpg".to_owned()),
                ..TrackMetadata::default()
            }],
            songs: Vec::new(),
        };
        let edges: Vec<u32> = LibrarySync::artwork_requests(&snapshot)
            .iter()
            .map(|r| r.edge)
            .collect();
        assert_eq!(edges, [LARGE_EDGE, SMALL_EDGE]);
    }

    #[test]
    fn next_offset_should_parse_relative_link() {
        assert_eq!(
            next_offset("/v1/me/library/playlists/p.1/tracks?l=en-GB&offset=300"),
            Some(300)
        );
    }

    #[test]
    fn no_related_resources_body_should_be_recognized() {
        assert!(is_no_related_resources(
            r#"{"errors":[{"status":"404","code":"40403","title":"No related resources"}]}"#
        ));
    }

    #[test]
    fn other_404_body_should_not_be_empty_list() {
        assert!(!is_no_related_resources(r#"{"errors":[{"code":"40400"}]}"#));
    }

    #[test]
    fn snapshot_should_keep_apple_order_and_duplicates() {
        let listing = Listing {
            items: vec![song("i.2", "B"), song("i.1", "A"), song("i.2", "B")],
            reported_total: Some(3),
        };
        let snapshot = build_snapshot(
            vec![(
                Playlist::minimal("p.x", CollectionKind::Playlist, "X"),
                listing,
            )],
            &Listing::default(),
        )
        .unwrap();
        assert_eq!(snapshot.collections[0].track_ids, ["i.2", "i.1", "i.2"]);
    }

    #[test]
    fn snapshot_should_dedupe_track_metadata() {
        let listing = Listing {
            items: vec![song("i.1", "A"), song("i.1", "A")],
            reported_total: Some(2),
        };
        let snapshot = build_snapshot(
            vec![(
                Playlist::minimal("p.x", CollectionKind::Playlist, "X"),
                listing,
            )],
            &Listing::default(),
        )
        .unwrap();
        assert_eq!(snapshot.tracks.len(), 1);
    }

    #[test]
    fn snapshot_should_reject_unparsable_entry() {
        let listing = Listing {
            items: vec![json!({"id": "i.1", "type": "library-songs"})],
            reported_total: Some(1),
        };
        assert!(
            build_snapshot(
                vec![(
                    Playlist::minimal("p.x", CollectionKind::Playlist, "X"),
                    listing,
                )],
                &Listing::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn snapshot_should_keep_songs_tab_order() {
        let songs = Listing {
            items: vec![song("i.9", "Z"), song("i.3", "A")],
            reported_total: Some(2),
        };
        let snapshot = build_snapshot(Vec::new(), &songs).unwrap();
        assert_eq!(snapshot.songs, ["i.9", "i.3"]);
    }

    #[test]
    fn snapshot_should_not_duplicate_song_also_in_playlist() {
        let songs = Listing {
            items: vec![song("i.1", "A")],
            reported_total: Some(1),
        };
        let playlist = Listing {
            items: vec![song("i.1", "A")],
            reported_total: Some(1),
        };
        let snapshot = build_snapshot(
            vec![(
                Playlist::minimal("p.x", CollectionKind::Playlist, "X"),
                playlist,
            )],
            &songs,
        )
        .unwrap();
        assert_eq!(snapshot.tracks.len(), 1);
    }
}
