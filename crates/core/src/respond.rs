//! Helpers for building locally produced responses and buffering upstream ones.

use bytes::Bytes;
use http_body_util::BodyExt;
use hudsucker::Body;
use hudsucker::hyper::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderName,
    HeaderValue, ORIGIN, TRANSFER_ENCODING, VARY,
};
use hudsucker::hyper::http::response::Parts;
use hudsucker::hyper::{HeaderMap, Response, StatusCode};

use crate::proxy::body::decode_if_supported;

/// Header marking responses produced by this proxy (value says from where).
pub(crate) const LOCAL_HEADER: HeaderName = HeaderName::from_static("x-cider-offline");

/// Headers that must not be replayed from a stored response.
const NOT_REPLAYED: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "set-cookie",
    "date",
    "age",
    "etag",
    "last-modified",
    "alt-svc",
    "strict-transport-security",
];

/// Returns whether a stored header may be sent again.
pub(crate) fn is_replayable_header(name: &str) -> bool {
    !NOT_REPLAYED.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// Adds permissive CORS headers echoing the request `Origin` (credentials
/// allowed), as Apple's API does for the web player.
pub(crate) fn add_cors(headers: &mut HeaderMap, request_headers: &HeaderMap) {
    let origin = request_headers
        .get(ORIGIN)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("*"));
    let credentialed = origin != "*";
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    if credentialed {
        headers.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        headers.append(VARY, HeaderValue::from_static("Origin"));
    }
    headers.insert(
        ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Length, Content-Range, Accept-Ranges"),
    );
}

/// Answer to a CORS preflight (`OPTIONS`).
pub(crate) fn preflight(request_headers: &HeaderMap) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    add_cors(headers, request_headers);
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, POST, PUT, DELETE, OPTIONS"),
    );
    let allowed = request_headers
        .get(ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("*"));
    headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, allowed);
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    headers.insert(LOCAL_HEADER, HeaderValue::from_static("preflight"));
    response
}

/// A JSON response with CORS headers, tagged with `source`.
pub(crate) fn json(
    status: StatusCode,
    value: &serde_json::Value,
    request_headers: &HeaderMap,
    source: &'static str,
) -> Response<Body> {
    bytes_response(
        status,
        "application/json; charset=utf-8",
        Bytes::from(value.to_string()),
        request_headers,
        source,
    )
}

/// A response with a fully buffered body, CORS headers and the local tag.
pub(crate) fn bytes_response(
    status: StatusCode,
    content_type: &str,
    body: Bytes,
    request_headers: &HeaderMap,
    source: &'static str,
) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(CONTENT_TYPE, value);
    }
    add_cors(headers, request_headers);
    headers.insert(LOCAL_HEADER, HeaderValue::from_static(source));
    response
}

/// Returns the `Content-Length` header value, if present and valid.
pub(crate) fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Returns the lower-cased `Content-Type` essence (without parameters).
pub(crate) fn content_type_essence(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    Some(
        value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
    )
}

/// A response whose (decoded) body was buffered for inspection.
pub(crate) struct Buffered {
    /// Status and headers (`Content-Encoding` / `Content-Length` removed when decoded).
    pub(crate) parts: Parts,
    /// The decoded body.
    pub(crate) body: Bytes,
}

impl Buffered {
    /// Rebuilds a response with the same status/headers and `body`.
    pub(crate) fn into_response_with(mut self, body: Bytes) -> Response<Body> {
        self.parts.headers.remove(CONTENT_LENGTH);
        self.parts.headers.remove(TRANSFER_ENCODING);
        Response::from_parts(self.parts, Body::from(body))
    }

    /// Rebuilds the response unchanged (decoded).
    pub(crate) fn into_response(self) -> Response<Body> {
        let body = self.body.clone();
        self.into_response_with(body)
    }
}

/// Outcome of [`buffer_decoded`].
pub(crate) enum BufferOutcome {
    /// The decoded body was buffered for inspection.
    Buffered(Buffered),
    /// The response could not be buffered and must be returned as is: either
    /// the untouched response (still encoded with an unsupported codec) or a
    /// `502` because the body stream failed after it was consumed.
    PassThrough(Response<Body>),
}

/// Decodes (if the encoding is supported) and buffers a response body.
pub(crate) async fn buffer_decoded(response: Response<Body>) -> BufferOutcome {
    let response = decode_if_supported(response);
    if response.headers().contains_key(CONTENT_ENCODING) {
        return BufferOutcome::PassThrough(response);
    }
    let (parts, body) = response.into_parts();
    match body.collect().await {
        Ok(collected) => BufferOutcome::Buffered(Buffered {
            parts,
            body: collected.to_bytes(),
        }),
        Err(error) => {
            tracing::debug!(%error, "failed to read upstream body");
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            BufferOutcome::PassThrough(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_origin() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://beta.music.apple.com"),
        );
        headers
    }

    #[test]
    fn cors_should_echo_origin() {
        let mut headers = HeaderMap::new();
        add_cors(&mut headers, &headers_with_origin());
        assert_eq!(
            headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://beta.music.apple.com"
        );
    }

    #[test]
    fn preflight_should_be_no_content() {
        assert_eq!(
            preflight(&headers_with_origin()).status(),
            StatusCode::NO_CONTENT
        );
    }

    #[test]
    fn content_encoding_should_not_be_replayed() {
        assert!(!is_replayable_header("Content-Encoding"));
    }

    #[test]
    fn content_type_should_be_replayed() {
        assert!(is_replayable_header("content-type"));
    }
}
