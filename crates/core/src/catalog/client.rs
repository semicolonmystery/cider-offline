//! HTTP client for the Apple Music catalog JSON:API.
//!
//! The client reads captured developer / media-user tokens from [`Storage`]
//! and talks directly to `amp-api.music.apple.com`. It is used by the GUI
//! (e.g. to refresh a playlist on demand), not by the proxy's hot path.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ORIGIN};
use serde_json::Value;

use crate::storage::{Storage, TokenKind};

use super::error::CatalogError;
use super::models::{Album, CatalogItem, Playlist, Song, SongCandidate};

const API_BASE: &str = "https://amp-api.music.apple.com";
const ORIGIN_VALUE: &str = "https://music.apple.com";

/// Client for the Apple Music catalog API.
#[derive(Debug, Clone)]
pub struct CatalogClient {
    storage: Storage,
    http: reqwest::Client,
}

impl CatalogClient {
    /// Creates a client with a default `reqwest::Client`.
    #[must_use]
    pub fn new(storage: Storage) -> Self {
        Self {
            storage,
            http: reqwest::Client::new(),
        }
    }

    /// Creates a client reusing an existing `reqwest::Client` (e.g. one built
    /// with custom timeouts or proxies).
    #[must_use]
    pub fn with_client(storage: Storage, http: reqwest::Client) -> Self {
        Self { storage, http }
    }

    /// Returns the two-letter storefront code for the authenticated user
    /// (e.g. `"us"`, `"gb"`).
    pub async fn storefront(&self) -> Result<String, CatalogError> {
        let url = format!("{API_BASE}/v1/me/storefront");
        let value = self.fetch_json(&url).await?;
        value
            .pointer("/data/0/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                CatalogError::InvalidResponse("missing /data/0/id in storefront response".into())
            })
    }

    /// Fetches an album by id, following the tracks pagination if any.
    pub async fn get_album(&self, id: &str) -> Result<Album, CatalogError> {
        let storefront = self.storefront().await?;
        // Apple rejects a bare `limit` on a single-resource GET (HTTP 400,
        // code 40004 "Limit may not be supplied on this request"). `include`
        // returns the first page of tracks inline; `follow_tracks` walks the
        // `next` links for the rest.
        let url = format!("{API_BASE}/v1/catalog/{storefront}/albums/{id}?include=tracks");
        let value = self.fetch_json(&url).await?;
        self.parse_album(&value).await
    }

    /// Fetches a catalog playlist by id, following pagination to collect all
    /// tracks.
    pub async fn get_playlist(&self, id: &str) -> Result<Playlist, CatalogError> {
        let storefront = self.storefront().await?;
        // A bare `limit` is rejected here too (see `get_album`); page through
        // the `next` links instead.
        let url = format!("{API_BASE}/v1/catalog/{storefront}/playlists/{id}?include=tracks");
        let value = self.fetch_json(&url).await?;
        self.parse_playlist(&value).await
    }

    /// Fetches a library playlist (the user's own playlist) by id.
    pub async fn get_library_playlist(&self, id: &str) -> Result<Playlist, CatalogError> {
        // As with the catalog endpoints, `limit` is rejected on this GET.
        let url = format!("{API_BASE}/v1/me/library/playlists/{id}?include=tracks");
        let value = self.fetch_json(&url).await?;
        self.parse_playlist(&value).await
    }

    /// Fetches a single song by id.
    pub async fn get_song(&self, id: &str) -> Result<Song, CatalogError> {
        let storefront = self.storefront().await?;
        let url = format!("{API_BASE}/v1/catalog/{storefront}/songs/{id}");
        let value = self.fetch_json(&url).await?;
        let resource = first_data(&value)?;
        parse_song(resource).ok_or_else(|| {
            CatalogError::InvalidResponse(format!("song {id} missing required fields"))
        })
    }

    /// Searches the catalog for playable songs matching `term` (at most
    /// `limit`, Apple caps it at 25).
    pub async fn search_songs(
        &self,
        term: &str,
        limit: u8,
    ) -> Result<Vec<SongCandidate>, CatalogError> {
        let storefront = self.storefront().await?;
        let url = format!("{API_BASE}/v1/catalog/{storefront}/search");
        let limit = limit.clamp(1, 25).to_string();
        let headers = self.auth_headers().await?;
        let response = self
            .http
            .get(&url)
            .headers(headers)
            .query(&[
                ("term", term),
                ("types", "songs"),
                ("limit", limit.as_str()),
            ])
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CatalogError::Status {
                status: status.as_u16(),
                body: body.chars().take(512).collect(),
            });
        }
        let document = response.json::<Value>().await?;
        Ok(document
            .pointer("/results/songs/data")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_candidate).collect())
            .unwrap_or_default())
    }

    /// Classifies `input` (full URL or bare id) and fetches the matching
    /// catalog item.
    pub async fn resolve(&self, input: &str) -> Result<CatalogItem, CatalogError> {
        match classify(input)? {
            Resolved::Album(id) => self.get_album(&id).await.map(CatalogItem::Album),
            Resolved::Playlist(id) => self.get_playlist(&id).await.map(CatalogItem::Playlist),
            Resolved::LibraryPlaylist(id) => self
                .get_library_playlist(&id)
                .await
                .map(CatalogItem::Playlist),
            Resolved::Song(id) => self.get_song(&id).await.map(CatalogItem::Song),
        }
    }

    /// Sends an authorized `GET` for an API `path` (e.g.
    /// `/v1/catalog/us/songs/1/lyrics`) and returns the raw response, whatever
    /// its status.
    pub(crate) async fn get_api(&self, path: &str) -> Result<reqwest::Response, CatalogError> {
        let headers = self.auth_headers().await?;
        Ok(self
            .http
            .get(format!("{API_BASE}{path}"))
            .headers(headers)
            .send()
            .await?)
    }

    // ---- internals ----

    async fn auth_headers(&self) -> Result<HeaderMap, CatalogError> {
        let dev = self
            .storage
            .get_auth_token(TokenKind::DeveloperToken)
            .await?
            .ok_or(CatalogError::MissingToken("developer_token"))?;
        let mut auth = HeaderValue::from_str(&dev).map_err(|_| CatalogError::InvalidHeader)?;
        // Mark as sensitive so it's not logged by reqwest's debug formatting.
        auth.set_sensitive(true);

        let mut headers = HeaderMap::with_capacity(3);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        headers.insert(ORIGIN, HeaderValue::from_static(ORIGIN_VALUE));

        if let Some(mut_token) = self
            .storage
            .get_auth_token(TokenKind::MediaUserToken)
            .await?
        {
            let mut value =
                HeaderValue::from_str(&mut_token).map_err(|_| CatalogError::InvalidHeader)?;
            value.set_sensitive(true);
            let name = HeaderName::from_static("media-user-token");
            headers.insert(name, value);
        }
        Ok(headers)
    }

    async fn fetch_json(&self, url: &str) -> Result<Value, CatalogError> {
        let headers = self.auth_headers().await?;
        let response = self.http.get(url).headers(headers).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_owned());
            let snippet: String = body.chars().take(512).collect();
            return Err(CatalogError::Status {
                status: status.as_u16(),
                body: snippet,
            });
        }
        Ok(response.json::<Value>().await?)
    }

    async fn parse_album(&self, document: &Value) -> Result<Album, CatalogError> {
        let resource = first_data(document)?;
        let id = str_at(resource, "/id")
            .ok_or_else(|| CatalogError::InvalidResponse("album missing id".into()))?
            .to_owned();
        let name = str_at(resource, "/attributes/name")
            .ok_or_else(|| CatalogError::InvalidResponse(format!("album {id} missing name")))?
            .to_owned();
        let artist = str_at(resource, "/attributes/artistName")
            .unwrap_or_default()
            .to_owned();
        let artwork_url = str_at(resource, "/attributes/artwork/url").map(str::to_owned);

        let mut tracks = collect_songs(resource.pointer("/relationships/tracks/data"));
        if let Some(next) = str_at(resource, "/relationships/tracks/next") {
            self.follow_tracks(next, &mut tracks).await?;
        }

        Ok(Album {
            id,
            name,
            artist,
            artwork_url,
            tracks,
        })
    }

    async fn parse_playlist(&self, document: &Value) -> Result<Playlist, CatalogError> {
        let resource = first_data(document)?;
        let id = str_at(resource, "/id")
            .ok_or_else(|| CatalogError::InvalidResponse("playlist missing id".into()))?
            .to_owned();
        let name = str_at(resource, "/attributes/name")
            .ok_or_else(|| CatalogError::InvalidResponse(format!("playlist {id} missing name")))?
            .to_owned();
        let curator = str_at(resource, "/attributes/curatorName").map(str::to_owned);
        let artwork_url = str_at(resource, "/attributes/artwork/url").map(str::to_owned);

        let mut tracks = collect_songs(resource.pointer("/relationships/tracks/data"));
        if let Some(next) = str_at(resource, "/relationships/tracks/next") {
            self.follow_tracks(next, &mut tracks).await?;
        }

        Ok(Playlist {
            id,
            name,
            curator,
            artwork_url,
            tracks,
        })
    }

    /// Follows the `next` chain for a playlist's / album's tracks, appending
    /// decoded songs to `tracks`.
    async fn follow_tracks(
        &self,
        initial_next: &str,
        tracks: &mut Vec<Song>,
    ) -> Result<(), CatalogError> {
        let mut next: Option<String> = Some(absolute_url(initial_next)?);
        // Guard against malformed `next` chains looping forever.
        let mut remaining_pages: u32 = 200;
        while let Some(url) = next.take() {
            if remaining_pages == 0 {
                return Err(CatalogError::InvalidResponse(
                    "tracks pagination exceeded 200 pages".into(),
                ));
            }
            remaining_pages -= 1;

            let page = self.fetch_json(&url).await?;
            tracks.extend(collect_songs(page.get("data")));
            next = page
                .get("next")
                .and_then(Value::as_str)
                .map(absolute_url)
                .transpose()?;
        }
        Ok(())
    }
}

fn first_data(document: &Value) -> Result<&Value, CatalogError> {
    document
        .pointer("/data/0")
        .ok_or_else(|| CatalogError::InvalidResponse("missing /data/0".into()))
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn parse_song(resource: &Value) -> Option<Song> {
    let id = str_at(resource, "/id")?.to_owned();
    let attrs = resource.pointer("/attributes")?;
    let title = str_at(attrs, "/name")?.to_owned();
    let artist = str_at(attrs, "/artistName").unwrap_or_default().to_owned();
    let album = str_at(attrs, "/albumName").map(str::to_owned);
    let duration_ms = attrs
        .get("durationInMillis")
        .and_then(Value::as_u64)
        .and_then(|d| u32::try_from(d).ok())
        .unwrap_or(0);
    let artwork_url = str_at(attrs, "/artwork/url").map(str::to_owned);
    Some(Song {
        id,
        title,
        artist,
        album,
        duration_ms,
        artwork_url,
    })
}

/// A search hit as a [`SongCandidate`]; `None` unless it is a playable song.
fn parse_candidate(resource: &Value) -> Option<SongCandidate> {
    let attrs = resource.pointer("/attributes")?;
    attrs.get("playParams")?;
    let number = |key: &str| {
        attrs
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    Some(SongCandidate {
        id: str_at(resource, "/id")?.to_owned(),
        title: str_at(attrs, "/name")?.to_owned(),
        artist: str_at(attrs, "/artistName").unwrap_or_default().to_owned(),
        album: str_at(attrs, "/albumName").map(str::to_owned),
        duration_ms: number("durationInMillis"),
        track_number: number("trackNumber"),
        isrc: str_at(attrs, "/isrc").map(str::to_owned),
    })
}

fn collect_songs(data: Option<&Value>) -> Vec<Song> {
    data.and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_song).collect())
        .unwrap_or_default()
}

fn absolute_url(raw: &str) -> Result<String, CatalogError> {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        Ok(raw.to_owned())
    } else if let Some(path) = raw.strip_prefix('/') {
        Ok(format!("{API_BASE}/{path}"))
    } else {
        Err(CatalogError::InvalidUrl(raw.to_owned()))
    }
}

// ---- URL / id classification ----

#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    Album(String),
    Playlist(String),
    LibraryPlaylist(String),
    Song(String),
}

fn classify(input: &str) -> Result<Resolved, CatalogError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CatalogError::UnrecognizedInput(input.to_owned()));
    }

    // Full music.apple.com URL
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return classify_url(trimmed);
    }

    // Short path like /albums/1234567 or albums/1234567
    let path = trimmed.trim_start_matches('/');
    for kind in [
        ("albums/", ResolvedKind::Album),
        ("playlists/", ResolvedKind::Playlist),
        ("songs/", ResolvedKind::Song),
        ("library/playlists/", ResolvedKind::LibraryPlaylist),
        ("me/library/playlists/", ResolvedKind::LibraryPlaylist),
    ] {
        if let Some(rest) = path.strip_prefix(kind.0) {
            let id = rest.split(['/', '?']).next().unwrap_or("");
            if id.is_empty() {
                continue;
            }
            return Ok(kind.1.with(id.to_owned()));
        }
    }

    // Bare id
    if let Some(stripped) = trimmed.strip_prefix("pl.")
        && !stripped.is_empty()
    {
        return Ok(Resolved::Playlist(trimmed.to_owned()));
    }
    if let Some(stripped) = trimmed.strip_prefix("p.")
        && !stripped.is_empty()
    {
        return Ok(Resolved::LibraryPlaylist(trimmed.to_owned()));
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Resolved::Song(trimmed.to_owned()));
    }
    // Last resort: assume album id (opaque strings can be album ids).
    Ok(Resolved::Album(trimmed.to_owned()))
}

#[derive(Copy, Clone)]
enum ResolvedKind {
    Album,
    Playlist,
    LibraryPlaylist,
    Song,
}

impl ResolvedKind {
    fn with(self, id: String) -> Resolved {
        match self {
            Self::Album => Resolved::Album(id),
            Self::Playlist => Resolved::Playlist(id),
            Self::LibraryPlaylist => Resolved::LibraryPlaylist(id),
            Self::Song => Resolved::Song(id),
        }
    }
}

fn classify_url(url: &str) -> Result<Resolved, CatalogError> {
    // Split off scheme+host, keeping path+query.
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (_host, path_and_query) = after_scheme
        .split_once('/')
        .map_or((after_scheme, ""), |(h, rest)| (h, rest));
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(p, q)| (p, q));

    // Library playlists: /library/playlist/<name>/<id>
    if let Some(id) = segment_after(path, "library/playlist/") {
        return Ok(Resolved::LibraryPlaylist(id));
    }
    // Standalone song page: /{storefront}/song/<name>/<id>
    if let Some(id) = segment_after(path, "/song/") {
        return Ok(Resolved::Song(id));
    }
    // Album with optional ?i=<song id>
    if let Some(id) = segment_after(path, "/album/") {
        if let Some(song_id) = query_param(query, "i") {
            return Ok(Resolved::Song(song_id));
        }
        return Ok(Resolved::Album(id));
    }
    // Playlist page: /{storefront}/playlist/<name>/<id>
    if let Some(id) = segment_after(path, "/playlist/") {
        if id.starts_with("p.") {
            return Ok(Resolved::LibraryPlaylist(id));
        }
        return Ok(Resolved::Playlist(id));
    }
    // JSON:API style paths used as full URLs. The leading slash was stripped
    // when splitting host from path, so match without it.
    for prefix in ["v1/catalog/", "v1/me/library/"] {
        if path.starts_with(prefix) {
            return classify_api_path(path);
        }
    }
    Err(CatalogError::UnrecognizedInput(url.to_owned()))
}

/// Returns the last non-empty path segment after `marker`, up to the next
/// slash. `marker` is matched case-sensitively.
fn segment_after(path: &str, marker: &str) -> Option<String> {
    let idx = path.find(marker)?;
    let rest = &path[idx + marker.len()..];
    // Take segments until we have two (name, id) or hit end.
    let mut last = None;
    for segment in rest.split('/').filter(|s| !s.is_empty()) {
        last = Some(segment);
    }
    last.map(str::to_owned)
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key && !v.is_empty() {
            Some(v.to_owned())
        } else {
            None
        }
    })
}

fn classify_api_path(path: &str) -> Result<Resolved, CatalogError> {
    // Expected segments:
    //   v1/catalog/<sf>/albums/<id>
    //   v1/catalog/<sf>/playlists/<id>
    //   v1/catalog/<sf>/songs/<id>
    //   v1/me/library/playlists/<id>
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for window in segments.windows(2) {
        let (kind, id) = (window[0], window[1]);
        if id.is_empty() {
            continue;
        }
        match kind {
            "albums" => return Ok(Resolved::Album(id.to_owned())),
            "songs" => return Ok(Resolved::Song(id.to_owned())),
            "playlists" => {
                if segments.contains(&"library") {
                    return Ok(Resolved::LibraryPlaylist(id.to_owned()));
                }
                return Ok(Resolved::Playlist(id.to_owned()));
            }
            _ => {}
        }
    }
    Err(CatalogError::UnrecognizedInput(path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_bare_numeric_id_is_song() {
        assert_eq!(
            classify("1234567").unwrap(),
            Resolved::Song("1234567".to_owned())
        );
    }

    #[test]
    fn classify_bare_catalog_playlist_id() {
        assert_eq!(
            classify("pl.u-ABCDEF").unwrap(),
            Resolved::Playlist("pl.u-ABCDEF".to_owned())
        );
    }

    #[test]
    fn classify_bare_library_playlist_id() {
        assert_eq!(
            classify("p.ABCDEFG").unwrap(),
            Resolved::LibraryPlaylist("p.ABCDEFG".to_owned())
        );
    }

    #[test]
    fn classify_bare_other_id_is_album() {
        assert_eq!(
            classify("ra.abc123").unwrap(),
            Resolved::Album("ra.abc123".to_owned())
        );
    }

    #[test]
    fn classify_short_album_path() {
        assert_eq!(
            classify("/albums/1234567").unwrap(),
            Resolved::Album("1234567".to_owned())
        );
    }

    #[test]
    fn classify_short_song_path() {
        assert_eq!(
            classify("songs/42").unwrap(),
            Resolved::Song("42".to_owned())
        );
    }

    #[test]
    fn classify_url_album_without_song_param() {
        assert_eq!(
            classify("https://music.apple.com/us/album/some-name/1234567").unwrap(),
            Resolved::Album("1234567".to_owned())
        );
    }

    #[test]
    fn classify_url_album_with_song_param_resolves_to_song() {
        let got = classify("https://music.apple.com/us/album/some-name/1234567?i=9876543").unwrap();
        assert_eq!(got, Resolved::Song("9876543".to_owned()));
    }

    #[test]
    fn classify_url_catalog_playlist() {
        assert_eq!(
            classify("https://music.apple.com/us/playlist/name/pl.u-ABCDEF").unwrap(),
            Resolved::Playlist("pl.u-ABCDEF".to_owned())
        );
    }

    #[test]
    fn classify_url_library_playlist() {
        assert_eq!(
            classify("https://music.apple.com/library/playlist/p.ABCDEFG").unwrap(),
            Resolved::LibraryPlaylist("p.ABCDEFG".to_owned())
        );
    }

    #[test]
    fn classify_url_song_page() {
        assert_eq!(
            classify("https://music.apple.com/us/song/name/1234567").unwrap(),
            Resolved::Song("1234567".to_owned())
        );
    }

    #[test]
    fn classify_api_path_album() {
        assert_eq!(
            classify("https://amp-api.music.apple.com/v1/catalog/us/albums/1234567").unwrap(),
            Resolved::Album("1234567".to_owned())
        );
    }

    #[test]
    fn classify_api_path_library_playlist() {
        assert_eq!(
            classify("https://amp-api.music.apple.com/v1/me/library/playlists/p.xxx").unwrap(),
            Resolved::LibraryPlaylist("p.xxx".to_owned())
        );
    }

    #[test]
    fn classify_empty_input_errors() {
        assert!(matches!(
            classify("   "),
            Err(CatalogError::UnrecognizedInput(_))
        ));
    }

    #[test]
    fn classify_playlist_url_with_library_prefix_in_name_still_catalog() {
        // Ensure we don't mis-classify a catalog URL that just happens to
        // contain 'library' somewhere in the name slug.
        let got = classify("https://music.apple.com/us/playlist/my-library-mix/pl.1234").unwrap();
        assert_eq!(got, Resolved::Playlist("pl.1234".to_owned()));
    }

    #[test]
    fn absolute_url_prepends_api_base_for_paths() {
        assert_eq!(
            absolute_url("/v1/catalog/us/playlists/pl.x/tracks?offset=100").unwrap(),
            format!("{API_BASE}/v1/catalog/us/playlists/pl.x/tracks?offset=100")
        );
    }

    #[test]
    fn absolute_url_passes_through_https() {
        let full = "https://amp-api.music.apple.com/v1/x";
        assert_eq!(absolute_url(full).unwrap(), full);
    }

    #[test]
    fn query_param_finds_key() {
        assert_eq!(query_param("a=1&i=42&b=2", "i").as_deref(), Some("42"));
        assert!(query_param("a=1&b=2", "i").is_none());
    }

    #[test]
    fn parse_song_reads_attributes() {
        let v = serde_json::json!({
            "id": "123",
            "attributes": {
                "name": "Title",
                "artistName": "Artist",
                "albumName": "Album",
                "durationInMillis": 240_000u64,
                "artwork": { "url": "https://x/{w}x{h}.jpg" }
            }
        });
        let song = parse_song(&v).unwrap();
        assert_eq!(song.id, "123");
        assert_eq!(song.title, "Title");
        assert_eq!(song.artist, "Artist");
        assert_eq!(song.album.as_deref(), Some("Album"));
        assert_eq!(song.duration_ms, 240_000);
        assert_eq!(song.artwork_url.as_deref(), Some("https://x/{w}x{h}.jpg"));
    }

    #[test]
    fn search_hit_without_play_params_is_not_a_candidate() {
        let hit = serde_json::json!({"id": "1", "attributes": {"name": "A", "artistName": "B"}});
        assert!(parse_candidate(&hit).is_none());
    }

    #[test]
    fn parse_song_without_attributes_returns_none() {
        let v = serde_json::json!({ "id": "123" });
        assert!(parse_song(&v).is_none());
    }
}
