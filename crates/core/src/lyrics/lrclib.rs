//! Minimal [LRCLIB](https://lrclib.net/docs) client.

use std::time::Duration;

use serde::Deserialize;

use super::LyricsError;

/// Public LRCLIB instance.
pub const LRCLIB_BASE_URL: &str = "https://lrclib.net";

/// `User-Agent` sent to LRCLIB: application name and version only.
pub const USER_AGENT: &str = concat!("CiderOffline/", env!("CARGO_PKG_VERSION"));

/// How far (seconds) a search result's duration may differ from the track's.
const DURATION_TOLERANCE_SECS: f64 = 2.0;

/// Track signature used to look up lyrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsQuery {
    /// Track title.
    pub title: String,
    /// Artist name.
    pub artist: String,
    /// Album name.
    pub album: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u32>,
    /// ISRC, when known (exact lookups where a provider supports them).
    pub isrc: Option<String>,
}

impl LyricsQuery {
    fn duration_secs(&self) -> Option<u32> {
        self.duration_ms
            .map(|ms| (ms + 500) / 1_000)
            .filter(|s| (1..=3600).contains(s))
    }
}

/// Lyrics found on LRCLIB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundLyrics {
    /// LRC with line timestamps.
    Synced(String),
    /// Unsynced text.
    Plain(String),
    /// The track is marked instrumental.
    Instrumental,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    instrumental: bool,
    duration: Option<f64>,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

impl Record {
    fn into_found(self) -> Option<FoundLyrics> {
        if let Some(synced) = self.synced_lyrics.filter(|s| !s.trim().is_empty()) {
            Some(FoundLyrics::Synced(synced))
        } else if let Some(plain) = self.plain_lyrics.filter(|s| !s.trim().is_empty()) {
            Some(FoundLyrics::Plain(plain))
        } else if self.instrumental {
            Some(FoundLyrics::Instrumental)
        } else {
            None
        }
    }

    fn duration_matches(&self, secs: Option<u32>) -> bool {
        match (self.duration, secs) {
            (Some(found), Some(wanted)) => {
                (found - f64::from(wanted)).abs() <= DURATION_TOLERANCE_SECS
            }
            _ => true,
        }
    }
}

/// LRCLIB HTTP client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LrclibClient {
    http: reqwest::Client,
    base_url: String,
}

impl LrclibClient {
    /// Client for `base_url` (normally [`LRCLIB_BASE_URL`]).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built (TLS backend).
    pub fn new(base_url: &str) -> Result<Self, LyricsError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// Looks up lyrics: `/api/get` by exact signature first, then
    /// `/api/search` (preferring synced results whose duration matches).
    ///
    /// # Errors
    ///
    /// Returns transport / HTTP status errors (a `404` from `/api/get` is not an error).
    pub async fn find(&self, query: &LyricsQuery) -> Result<Option<FoundLyrics>, LyricsError> {
        if let Some(found) = self.get(query).await? {
            return Ok(Some(found));
        }
        self.search(query).await
    }

    async fn get(&self, query: &LyricsQuery) -> Result<Option<FoundLyrics>, LyricsError> {
        let mut params: Vec<(&str, String)> = vec![
            ("track_name", query.title.clone()),
            ("artist_name", query.artist.clone()),
        ];
        if let Some(album) = &query.album {
            params.push(("album_name", album.clone()));
        }
        if let Some(secs) = query.duration_secs() {
            params.push(("duration", secs.to_string()));
        }
        let response = self
            .http
            .get(format!("{}/api/get", self.base_url))
            .query(&params)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let record: Record = check_status(response)?.json().await?;
        Ok(record.into_found())
    }

    async fn search(&self, query: &LyricsQuery) -> Result<Option<FoundLyrics>, LyricsError> {
        let response = self
            .http
            .get(format!("{}/api/search", self.base_url))
            .query(&[
                ("track_name", query.title.as_str()),
                ("artist_name", query.artist.as_str()),
            ])
            .send()
            .await?;
        let records: Vec<Record> = check_status(response)?.json().await?;
        let secs = query.duration_secs();
        let (matching, other): (Vec<Record>, Vec<Record>) =
            records.into_iter().partition(|r| r.duration_matches(secs));
        let mut candidates: Vec<FoundLyrics> = matching
            .into_iter()
            .chain(other)
            .filter_map(Record::into_found)
            .collect();
        // Stable sort keeps the duration-matching ones first within each class.
        candidates.sort_by_key(|found| match found {
            FoundLyrics::Synced(_) => 0,
            FoundLyrics::Plain(_) => 1,
            FoundLyrics::Instrumental => 2,
        });
        Ok(candidates.into_iter().next())
    }
}

/// Passes a success response through; otherwise returns
/// [`LyricsError::Status`] with the server's `Retry-After` (delta seconds).
fn check_status(response: reqwest::Response) -> Result<reqwest::Response, LyricsError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(LyricsError::Status {
        status: status.as_u16(),
        retry_after: retry_after(response.headers()),
    })
}

/// Parses a `Retry-After` header given in seconds (the HTTP-date form is
/// ignored).
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// HTTP/1.1 server answering requests in order with the given (status, body),
    /// recording request heads.
    async fn serve(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut heads = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                heads.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            heads
        });
        (format!("http://{addr}"), handle)
    }

    fn query() -> LyricsQuery {
        LyricsQuery {
            title: "I Want to Live".to_owned(),
            artist: "Borislav Slavov".to_owned(),
            album: None,
            duration_ms: Some(233_000),
            isrc: None,
        }
    }

    #[tokio::test]
    async fn get_hit_should_return_synced_lyrics() {
        let (base, server) = serve(vec![(
            200,
            r#"{"duration":233,"instrumental":false,"plainLyrics":"a","syncedLyrics":"[00:01.00]a"}"#,
        )])
        .await;
        let found = LrclibClient::new(&base)
            .unwrap()
            .find(&query())
            .await
            .unwrap();
        assert_eq!(found, Some(FoundLyrics::Synced("[00:01.00]a".to_owned())));
        let heads = server.await.unwrap();
        assert!(heads[0].starts_with("GET /api/get?track_name=I+Want+to+Live"));
    }

    #[tokio::test]
    async fn get_miss_should_fall_back_to_search() {
        let (base, server) = serve(vec![
            (404, r#"{"statusCode":404}"#),
            (
                200,
                r#"[{"duration":100,"syncedLyrics":"[00:01.00]wrong"},{"duration":233.4,"syncedLyrics":"[00:01.00]right"}]"#,
            ),
        ])
        .await;
        let found = LrclibClient::new(&base)
            .unwrap()
            .find(&query())
            .await
            .unwrap();
        assert_eq!(
            found,
            Some(FoundLyrics::Synced("[00:01.00]right".to_owned()))
        );
        drop(server);
    }

    #[tokio::test]
    async fn user_agent_should_name_app_only() {
        let (base, server) = serve(vec![(200, r#"{"instrumental":true}"#)]).await;
        LrclibClient::new(&base)
            .unwrap()
            .find(&query())
            .await
            .unwrap();
        let heads = server.await.unwrap();
        assert!(
            heads[0]
                .to_ascii_lowercase()
                .contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase()))
        );
    }

    #[tokio::test]
    async fn rate_limit_should_report_status_and_retry_after() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 429 X
retry-after: 7
content-length: 0
connection: close

",
                )
                .await
                .unwrap();
        });
        let error = LrclibClient::new(&format!("http://{addr}"))
            .unwrap()
            .find(&query())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            LyricsError::Status {
                status: 429,
                retry_after: Some(d)
            } if d == Duration::from_secs(7)
        ));
        server.await.unwrap();
    }
}
