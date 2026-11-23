//! Optional capture of Apple-host request/response pairs to JSON files.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hudsucker::Body;
use hudsucker::hyper::header::CONTENT_TYPE;
use hudsucker::hyper::{HeaderMap, Request, Response, StatusCode};
use serde::Serialize;
use tokio_util::task::TaskTracker;

use super::body::{decode_if_supported, is_textual, read_body};

/// Headers whose values are replaced by [`REDACTED`] in capture files, in
/// requests and responses alike: the developer token (`authorization`), the
/// user token under every name Apple's clients send it, and cookies.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "media-user-token",
    "music-user-token",
    "x-apple-music-user-token",
];
const REDACTED: &str = "<redacted>";

/// Captured body representation.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CapturedBody {
    Empty,
    Text {
        text: String,
    },
    Omitted {
        reason: &'static str,
        content_type: Option<String>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct CapturedRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: CapturedBody,
}

#[derive(Debug, Serialize)]
pub(crate) struct CapturedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: CapturedBody,
}

#[derive(Debug, Serialize)]
struct CaptureRecord<'a> {
    id: u64,
    timestamp_ms: u128,
    duration_ms: u128,
    served_locally: bool,
    request: &'a CapturedRequest,
    response: Option<&'a CapturedResponse>,
}

fn redacted_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = if SECRET_HEADERS.contains(&name.as_str()) {
                REDACTED.to_owned()
            } else {
                String::from_utf8_lossy(value.as_bytes()).into_owned()
            };
            (name.as_str().to_owned(), value)
        })
        .collect()
}

fn content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

/// Buffers a textual body for the record and returns an equivalent body to forward.
async fn capture_body(headers: &HeaderMap, body: Body) -> (Body, Result<CapturedBody, String>) {
    if !is_textual(headers) {
        let omitted = CapturedBody::Omitted {
            reason: "non-textual content type",
            content_type: content_type(headers),
        };
        return (body, Ok(omitted));
    }
    match read_body(body).await {
        Ok(bytes) if bytes.is_empty() => (Body::empty(), Ok(CapturedBody::Empty)),
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (Body::from(bytes), Ok(CapturedBody::Text { text }))
        }
        Err(error) => (Body::empty(), Err(error.to_string())),
    }
}

/// Buffers (if textual) and records a request.
pub(crate) async fn capture_request(request: Request<Body>) -> (Request<Body>, CapturedRequest) {
    let (parts, body) = request.into_parts();
    let (body, captured) = capture_body(&parts.headers, body).await;
    let record = CapturedRequest {
        method: parts.method.to_string(),
        url: parts.uri.to_string(),
        headers: redacted_headers(&parts.headers),
        body: captured.unwrap_or_else(|message| CapturedBody::Error { message }),
    };
    (Request::from_parts(parts, body), record)
}

/// Decodes (if possible), buffers (if textual) and records a response.
///
/// If the upstream body fails mid-stream the client receives `502`.
pub(crate) async fn capture_response(
    response: Response<Body>,
) -> (Response<Body>, CapturedResponse) {
    let (mut parts, body) = decode_if_supported(response).into_parts();
    let (body, captured) = capture_body(&parts.headers, body).await;
    let body_record = match captured {
        Ok(body) => body,
        Err(message) => {
            parts.status = StatusCode::BAD_GATEWAY;
            CapturedBody::Error { message }
        }
    };
    let record = CapturedResponse {
        status: parts.status.as_u16(),
        headers: redacted_headers(&parts.headers),
        body: body_record,
    };
    (Response::from_parts(parts, body), record)
}

/// A request that is being captured, waiting for its response.
#[derive(Debug)]
pub(crate) struct PendingCapture {
    pub(crate) request: CapturedRequest,
}

/// Writes capture records to `<data>/captures/`.
#[derive(Debug, Clone)]
pub(crate) struct CaptureWriter {
    dir: PathBuf,
    tasks: TaskTracker,
}

impl CaptureWriter {
    pub(crate) fn new(dir: PathBuf, tasks: TaskTracker) -> Self {
        Self { dir, tasks }
    }

    /// Serializes and writes one record in the background.
    pub(crate) fn write(
        &self,
        exchange: &super::Exchange,
        pending: &PendingCapture,
        response: Option<&CapturedResponse>,
    ) {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let record = CaptureRecord {
            id: exchange.id(),
            timestamp_ms,
            duration_ms: exchange.started().elapsed().as_millis(),
            served_locally: exchange.served_locally(),
            request: &pending.request,
            response,
        };
        let json = match serde_json::to_vec_pretty(&record) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(%error, "failed to serialize capture");
                return;
            }
        };
        let path = self
            .dir
            .join(file_name(timestamp_ms, exchange.id(), exchange.host()));
        let dir = self.dir.clone();
        self.tasks.spawn(async move {
            if let Err(error) = write_file(&dir, &path, &json).await {
                tracing::warn!(path = %path.display(), %error, "failed to write capture");
            }
        });
    }
}

async fn write_file(dir: &Path, path: &Path, contents: &[u8]) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::write(path, contents).await
}

/// `<timestamp>-<id>-<host>.json` with the host reduced to safe characters.
fn file_name(timestamp_ms: u128, id: u64, host: &str) -> String {
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{timestamp_ms}-{id:06}-{host}.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hudsucker::hyper::header::HeaderValue;

    #[test]
    fn secret_headers_should_be_redacted() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        headers.insert("accept", HeaderValue::from_static("*/*"));
        let redacted = redacted_headers(&headers);
        assert!(!redacted.iter().any(|(_, v)| v.contains("secret")));
    }

    const SECRET: &str = "s3cr3t-token-value";

    #[tokio::test]
    async fn every_secret_request_header_should_be_redacted() {
        for name in SECRET_HEADERS {
            let request = Request::builder()
                .uri("https://amp-api.music.apple.com/v1/me/library/songs")
                .header(*name, SECRET)
                .header("accept", "application/json")
                .body(Body::empty())
                .unwrap();
            let (_, captured) = capture_request(request).await;
            let json = serde_json::to_string(&captured).unwrap();
            assert!(!json.contains(SECRET), "{name} leaked: {json}");
            assert!(json.contains(REDACTED), "{name} not marked: {json}");
        }
    }

    #[tokio::test]
    async fn every_secret_response_header_should_be_redacted() {
        for name in SECRET_HEADERS {
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(*name, SECRET)
                .body(Body::empty())
                .unwrap();
            let (_, captured) = capture_response(response).await;
            let json = serde_json::to_string(&captured).unwrap();
            assert!(!json.contains(SECRET), "{name} leaked: {json}");
        }
    }

    #[test]
    fn apple_music_user_token_header_should_be_redacted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-apple-music-user-token", HeaderValue::from_static(SECRET));
        assert_eq!(
            redacted_headers(&headers),
            [("x-apple-music-user-token".to_owned(), REDACTED.to_owned())]
        );
    }

    #[test]
    fn ordinary_headers_should_be_kept() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("*/*"));
        assert_eq!(
            redacted_headers(&headers),
            [("accept".to_owned(), "*/*".to_owned())]
        );
    }

    #[test]
    fn file_name_should_sanitize_host() {
        assert_eq!(file_name(1, 2, "a/b:c"), "1-000002-a_b_c.json");
    }

    #[tokio::test]
    async fn textual_request_body_should_be_preserved() {
        let request = Request::builder()
            .uri("https://amp-api.music.apple.com/x")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from("{\"a\":1}"))
            .unwrap();
        let (request, _) = capture_request(request).await;
        let bytes = read_body(request.into_body()).await.unwrap();
        assert_eq!(&bytes[..], b"{\"a\":1}");
    }
}
