use std::time::Duration;

use cider_offline_core::download::DownloadError;
use cider_offline_core::storage::AudioVariant;

/// Errors that can occur during DRM-protected audio download and decryption.
#[derive(Debug, thiserror::Error)]
pub enum DrmError {
    /// Auth tokens have not been captured yet (play a song in Cider first).
    #[error("auth tokens not captured yet — play a song online in Cider first")]
    MissingTokens,
    /// Apple Music responded with a stale-session marker; the user must play a
    /// song online in Cider so the proxy can capture fresh tokens.
    #[error("Apple Music session expired — replay a song in Cider online to refresh tokens")]
    SessionExpired,
    /// A `play.itunes.apple.com` / CDN response with a non-success status.
    ///
    /// Headers (especially `Retry-After`) are extracted BEFORE the body is
    /// consumed, so core can honour them when retrying. The body text itself
    /// is never carried on the error — only its length — to keep secrets and
    /// server-side detail out of logs and UI messages.
    #[error("{endpoint} HTTP {status}, {body_len} bytes")]
    HttpStatus {
        /// Logical endpoint label (`webPlayback`, `widevineCert`, `license`,
        /// `playlist`, `segment`).
        endpoint: &'static str,
        /// HTTP status code of the response.
        status: u16,
        /// Length in bytes of the response body as read.
        body_len: usize,
        /// Value of `Retry-After` in seconds, when present and parsable.
        retry_after: Option<Duration>,
    },
    /// The license response had no license (empty / non-JSON body).
    ///
    /// Apple returns HTTP 200 with an empty or non-JSON body when it is
    /// overloaded. Treated as [`DownloadError::Transient`] — not
    /// [`DownloadError::Unavailable`] — because pacing by core clears it.
    #[error("license HTTP {status}, {body_len} bytes, no license")]
    EmptyLicense {
        /// HTTP status (typically 200).
        status: u16,
        /// Length in bytes of the response body.
        body_len: usize,
    },
    /// webPlayback returned no usable audio asset.
    ///
    /// Carries a reason worded by what Apple returned (e.g. the flavors seen,
    /// or `"music video: hls-playlist-url only"`).
    #[error("webPlayback returned no ctrp audio asset ({0})")]
    NoAudioAsset(String),
    /// The song has no such version: no enhancedHls master, or a master
    /// without the binaural (spatial) variant.
    #[error("no {0} version of this song")]
    VariantUnavailable(AudioVariant),
    /// webPlayback returned an empty `songList`.
    ///
    /// Observed with unlinked library ids (`i.…`). The argument is the
    /// `adam_id` that was requested, for operator diagnostics.
    #[error("webPlayback returned an empty songList for {0}")]
    EmptySongList(String),
    /// [`cider_offline_core::download::DownloadRequest::cancel`] fired.
    #[error("cancelled")]
    Cancelled,
    /// The playback response was missing the song-level `widevine-cert-url`.
    #[error("Apple Music webPlayback response missing widevine-cert-url (not a Widevine track)")]
    MissingCertUrl,
    /// The HLS manifest is missing the expected Widevine key URI.
    #[error("HLS manifest missing Widevine key URI")]
    MissingKeyUri,
    /// The HLS manifest has no media segments.
    #[error("HLS manifest has no media segments")]
    MissingSegments,
    /// The HLS manifest is missing the expected `#EXT-X-MAP` init segment URI.
    #[error("HLS manifest missing EXT-X-MAP init segment")]
    MissingInitSegment,
    /// The HLS manifest could not be parsed.
    #[error("failed to parse HLS manifest: {0}")]
    HlsParse(String),
    /// The response from the HLS manifest URL could not be parsed as HLS at
    /// all — e.g. an HTML error page, a redirect body, or an unexpected
    /// payload. Separate from [`DrmError::HlsParse`] because the dumped
    /// diagnostic (see WARN log) is what the operator should look at first.
    #[error("{0}")]
    ManifestParseFailure(String),
    /// Resolving a relative HLS URI failed.
    #[error("invalid HLS URI: {0}")]
    InvalidUri(String),
    /// The Widevine Device blob is invalid or corrupt.
    #[error("invalid WVD data: {0}")]
    InvalidWvd(String),
    /// A Widevine license negotiation error.
    #[error("Widevine license error: {0}")]
    License(String),
    /// The license server returned an `ERROR_RESPONSE` `SignedMessage`.
    /// Separate from [`DrmError::License`] so a live-run diagnostic can
    /// distinguish a server-side rejection from a local CDM / parser bug.
    #[error("Widevine license server returned error: {0}")]
    LicenseServerError(String),
    /// The expected content key was not found in the license response.
    #[error("content key not found for key id {0}")]
    KeyNotFound(String),
    /// CENC decryption of the audio data failed.
    #[error("CENC decryption failed: {0}")]
    Decrypt(String),
    /// A 206 Partial Content response's `Content-Range` (or `Content-Length`)
    /// did not match the planned byte range.
    ///
    /// Treated as [`DownloadError::Transient`] because it is typically a CDN
    /// quirk that clears on a retry. Carries a short diagnostic string with
    /// the planned window and what the server returned.
    #[error("CDN returned 206 with unexpected range: {0}")]
    RangeMismatch(String),
    /// An error returned by the Apple Music API that does not fit any of the
    /// typed variants above.
    #[error("Apple Music API error: {0}")]
    Api(String),
    /// A protobuf decoding error.
    #[error("protobuf error: {0}")]
    Proto(#[from] prost::DecodeError),
    /// An RSA cryptographic error.
    #[error("RSA error: {0}")]
    Rsa(#[from] rsa::Error),
    /// An HTTP request error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// A filesystem I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A persistent storage error.
    #[error("storage error: {0}")]
    Storage(String),
    /// A background task failed to join.
    #[error("background task failed: {0}")]
    Join(String),
}

impl From<cider_offline_core::StorageError> for DrmError {
    fn from(err: cider_offline_core::StorageError) -> Self {
        Self::Storage(err.to_string())
    }
}

impl From<DrmError> for DownloadError {
    fn from(err: DrmError) -> Self {
        let message = err.to_string();
        match err {
            DrmError::MissingTokens | DrmError::SessionExpired => {
                DownloadError::auth_expired(message)
            }
            DrmError::HttpStatus {
                status: 401 | 403, ..
            } => DownloadError::auth_expired(message),
            DrmError::HttpStatus {
                status,
                retry_after,
                ..
            } if status == 429 || (500..=599).contains(&status) => {
                DownloadError::transient_http(status, retry_after, message)
            }
            DrmError::EmptyLicense { status, .. } => {
                DownloadError::transient_http(status, None, message)
            }
            DrmError::Http(_) | DrmError::RangeMismatch(_) => DownloadError::transient(message),
            DrmError::NoAudioAsset(_) | DrmError::EmptySongList(_) => {
                DownloadError::unavailable(message)
            }
            DrmError::Cancelled => DownloadError::Cancelled,
            DrmError::VariantUnavailable(variant) => DownloadError::variant_unavailable(variant),

            DrmError::Io(e) => DownloadError::Io(e),
            DrmError::InvalidWvd(_)
            | DrmError::Decrypt(_)
            | DrmError::KeyNotFound(_)
            | DrmError::LicenseServerError(_)
            | DrmError::MissingKeyUri
            | DrmError::MissingInitSegment
            | DrmError::MissingSegments => DownloadError::unavailable(message),
            other => DownloadError::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cider_offline_core::download::DownloadError;

    use super::DrmError;

    #[test]
    fn http_status_429_maps_to_transient_with_retry_after() {
        let err = DrmError::HttpStatus {
            endpoint: "webPlayback",
            status: 429,
            body_len: 0,
            retry_after: Some(Duration::from_secs(5)),
        };
        match DownloadError::from(err) {
            DownloadError::Transient {
                status,
                retry_after,
                ..
            } => {
                assert_eq!(status, Some(429));
                assert_eq!(retry_after, Some(Duration::from_secs(5)));
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn http_status_401_maps_to_auth_expired() {
        let err = DrmError::HttpStatus {
            endpoint: "webPlayback",
            status: 401,
            body_len: 0,
            retry_after: None,
        };
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::AuthExpired { .. }
        ));
    }

    #[test]
    fn http_status_403_maps_to_auth_expired() {
        let err = DrmError::HttpStatus {
            endpoint: "license",
            status: 403,
            body_len: 0,
            retry_after: None,
        };
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::AuthExpired { .. }
        ));
    }

    #[test]
    fn http_status_500_maps_to_transient() {
        let err = DrmError::HttpStatus {
            endpoint: "widevineCert",
            status: 500,
            body_len: 0,
            retry_after: None,
        };
        match DownloadError::from(err) {
            DownloadError::Transient {
                status,
                retry_after,
                ..
            } => {
                assert_eq!(status, Some(500));
                assert_eq!(retry_after, None);
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn empty_license_maps_to_transient_with_license_status() {
        let err = DrmError::EmptyLicense {
            status: 200,
            body_len: 0,
        };
        match DownloadError::from(err) {
            DownloadError::Transient {
                status,
                retry_after,
                ..
            } => {
                assert_eq!(status, Some(200));
                assert_eq!(retry_after, None);
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn no_audio_asset_maps_to_unavailable() {
        let err = DrmError::NoAudioAsset("flavors: 30:cbcp256".into());
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::Unavailable { .. }
        ));
    }

    #[test]
    fn empty_song_list_maps_to_unavailable() {
        let err = DrmError::EmptySongList("1443063973".into());
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::Unavailable { .. }
        ));
    }

    #[test]
    fn session_expired_maps_to_auth_expired() {
        assert!(matches!(
            DownloadError::from(DrmError::SessionExpired),
            DownloadError::AuthExpired { .. }
        ));
    }

    #[test]
    fn missing_tokens_maps_to_auth_expired() {
        assert!(matches!(
            DownloadError::from(DrmError::MissingTokens),
            DownloadError::AuthExpired { .. }
        ));
    }

    #[test]
    fn range_mismatch_maps_to_transient() {
        let err = DrmError::RangeMismatch("planned=0-99 got=0-50".into());
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::Transient { .. }
        ));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        assert!(matches!(
            DownloadError::from(DrmError::Cancelled),
            DownloadError::Cancelled
        ));
    }
}
