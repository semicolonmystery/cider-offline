//! Error type for the Apple Music catalog client.

use crate::storage::StorageError;

/// Errors returned by [`crate::catalog::CatalogClient`] operations.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// Required auth token is missing from storage (not yet captured from a
    /// live Cider session).
    #[error("missing auth token: {0}")]
    MissingToken(&'static str),
    /// The storage layer failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    /// The HTTP request failed (network, TLS, timeout, …).
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// The Apple Music API returned a non-success status.
    #[error("apple music returned {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// First bytes of the response body, for diagnostics.
        body: String,
    },
    /// The response could not be decoded as JSON or was missing expected fields.
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    /// JSON decoding failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A captured auth header value contained non-ASCII bytes and cannot be
    /// sent in a header.
    #[error("invalid auth header value")]
    InvalidHeader,
    /// The caller passed a string that could not be classified as any known
    /// Apple Music item.
    #[error("unrecognized catalog item: {0}")]
    UnrecognizedInput(String),
    /// Building the HTTP request URL failed.
    #[error("invalid url: {0}")]
    InvalidUrl(String),
}
