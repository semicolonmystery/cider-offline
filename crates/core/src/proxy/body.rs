//! Helpers for buffering and decoding message bodies, for interceptors that
//! need to inspect them (capture, cache).

use bytes::Bytes;
use http_body_util::BodyExt;
use hudsucker::Body;
use hudsucker::hyper::header::{CONTENT_ENCODING, CONTENT_TYPE};
use hudsucker::hyper::{HeaderMap, Response};

/// Content encodings `hudsucker::decode_response` can undo.
const DECODABLE_ENCODINGS: &[&str] = &["identity", "gzip", "x-gzip", "deflate", "br", "zstd"];

/// Returns whether the `Content-Type` is textual (JSON, XML, text, form, …),
/// i.e. reasonably small and worth buffering for inspection.
pub fn is_textual(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence.starts_with("text/")
        || essence.ends_with("/json")
        || essence.ends_with("+json")
        || essence.ends_with("/xml")
        || essence.ends_with("+xml")
        || essence == "application/javascript"
        || essence == "application/x-www-form-urlencoded"
        || essence == "application/x-apple-plist"
        // HLS playlists (`.m3u8`) are small text manifests worth capturing for
        // DRM debugging, even though their content types are not obviously textual.
        || essence == "application/x-mpegurl"
        || essence == "application/vnd.apple.mpegurl"
        || essence == "audio/mpegurl"
        || essence == "audio/x-mpegurl"
}

/// Returns whether every listed `Content-Encoding` can be decoded.
pub fn can_decode(headers: &HeaderMap) -> bool {
    headers.get_all(CONTENT_ENCODING).iter().all(|value| {
        value.to_str().is_ok_and(|v| {
            v.split(',').map(str::trim).all(|enc| {
                enc.is_empty()
                    || DECODABLE_ENCODINGS
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(enc))
            })
        })
    })
}

/// Decodes a compressed response body if all its encodings are supported;
/// otherwise returns it untouched. On success `Content-Encoding` and
/// `Content-Length` are removed.
pub fn decode_if_supported(response: Response<Body>) -> Response<Body> {
    if !response.headers().contains_key(CONTENT_ENCODING) || !can_decode(response.headers()) {
        return response;
    }
    // Every encoding was checked above, and those are exactly the ones the
    // decoder accepts, so this cannot fail; the fallback keeps the type total.
    hudsucker::decode_response(response).unwrap_or_else(|error| {
        tracing::warn!(%error, "response decoding failed");
        Response::new(Body::empty())
    })
}

/// Buffers a whole body into memory.
///
/// # Errors
///
/// Returns the underlying transport error if the body stream fails.
pub async fn read_body(body: Body) -> Result<Bytes, hudsucker::Error> {
    Ok(body.collect().await?.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hudsucker::hyper::header::HeaderValue;

    fn with(name: hudsucker::hyper::header::HeaderName, value: &'static str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(name, HeaderValue::from_static(value));
        map
    }

    #[test]
    fn json_with_charset_should_be_textual() {
        assert!(is_textual(&with(
            CONTENT_TYPE,
            "application/json; charset=utf-8"
        )));
    }

    #[test]
    fn audio_should_not_be_textual() {
        assert!(!is_textual(&with(CONTENT_TYPE, "audio/mp4")));
    }

    #[test]
    fn hls_playlist_should_be_textual() {
        assert!(is_textual(&with(
            CONTENT_TYPE,
            "application/x-mpegURL; charset=utf-8"
        )));
    }

    #[test]
    fn binary_segment_should_not_be_textual() {
        assert!(!is_textual(&with(CONTENT_TYPE, "application/octet-stream")));
    }

    #[test]
    fn gzip_should_be_decodable() {
        assert!(can_decode(&with(CONTENT_ENCODING, "gzip")));
    }

    #[test]
    fn unknown_encoding_should_not_be_decodable() {
        assert!(!can_decode(&with(CONTENT_ENCODING, "gzip, compress")));
    }
}
