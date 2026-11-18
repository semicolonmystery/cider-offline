//! Lyrics providers: the sources [`super::LyricsEngine`] asks, in the user's
//! priority order.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::OnceCell;

use super::lrclib::retry_after;
use super::{
    FoundLyrics, LrclibClient, LyricsError, LyricsKind, LyricsProviderId, LyricsQuality,
    LyricsQuery, interceptor::apple_ttml, lrc_to_ttml, plain_to_ttml, ttml_quality,
};
use crate::cache::STOREFRONT_SETTING;
use crate::catalog::{CatalogClient, CatalogError};
use crate::storage::Storage;

/// Timeout of one Apple lyrics request.
const APPLE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a provider is asked for.
#[derive(Debug, Clone, Copy)]
pub struct LyricsRequest<'a> {
    /// Numeric Apple catalog id, when the track has one.
    pub catalog_id: Option<&'a str>,
    /// Title / artist / album / duration signature.
    pub query: &'a LyricsQuery,
}

/// Lyrics a provider found, already converted to TTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedLyrics {
    /// The TTML document.
    pub ttml: String,
    /// Which file it is stored as (word-timed files are Apple-only).
    pub kind: LyricsKind,
    /// How finely it is timed.
    pub quality: LyricsQuality,
    /// What the provider matched (its id, artist, title, duration), for
    /// diagnostics; `None` when the lookup was by id.
    pub matched: Option<String>,
}

/// A lookup that did not give an answer. "No lyrics" is not an error
/// (`Ok(None)`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// Worth retrying later: rate limit (429), server error (5xx), timeout
    /// or network error.
    #[error("{reason}")]
    Retryable {
        /// Human-readable cause.
        reason: String,
        /// How long the provider asked us to wait (`Retry-After`).
        retry_after: Option<Duration>,
    },
    /// Retrying will not help (bad request, rejected sign-in, bad data).
    #[error("{0}")]
    Fatal(String),
}

impl ProviderError {
    /// Whether the policy may retry it.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    pub(crate) fn from_status(status: u16, retry_after: Option<Duration>) -> Self {
        if status == 429 || (500..600).contains(&status) {
            Self::Retryable {
                reason: if status == 429 {
                    "rate limited (HTTP 429)".to_owned()
                } else {
                    format!("server error (HTTP {status})")
                },
                retry_after,
            }
        } else {
            Self::Fatal(format!("HTTP {status}"))
        }
    }

    pub(crate) fn from_reqwest(error: &reqwest::Error) -> Self {
        if let Some(status) = error.status() {
            return Self::from_status(status.as_u16(), None);
        }
        if error.is_decode() {
            return Self::Fatal(format!("unreadable response: {error}"));
        }
        let reason = if error.is_timeout() {
            "timed out".to_owned()
        } else {
            format!("network error: {error}")
        };
        Self::Retryable {
            reason,
            retry_after: None,
        }
    }
}

/// A source of lyrics.
#[async_trait]
pub trait LyricsProvider: Send + Sync + std::fmt::Debug {
    /// Which provider this is (its settings entry).
    fn id(&self) -> LyricsProviderId;

    /// The best quality this provider can ever return; the engine skips it
    /// once it has lyrics at least this good.
    fn best_quality(&self) -> LyricsQuality;

    /// Looks lyrics up. `Ok(None)` means the provider has none.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the lookup could not be answered.
    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError>;
}

/// [LRCLIB](https://lrclib.net) lookups (LRC converted to TTML).
#[derive(Debug, Clone)]
pub struct LrclibProvider {
    client: LrclibClient,
}

impl LrclibProvider {
    /// Provider over `client`.
    pub fn new(client: LrclibClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl LyricsProvider for LrclibProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Lrclib
    }

    fn best_quality(&self) -> LyricsQuality {
        // Enhanced-LRC word tags are dropped by the conversion.
        LyricsQuality::Line
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let found = self.client.find(request.query).await.map_err(|e| match e {
            LyricsError::Status {
                status,
                retry_after,
            } => ProviderError::from_status(status, retry_after),
            LyricsError::Http(error) => ProviderError::from_reqwest(&error),
            other => ProviderError::Fatal(other.to_string()),
        })?;
        let duration = request.query.duration_ms.map(u64::from);
        Ok(match found {
            Some(FoundLyrics::Synced(lrc)) => {
                lrc_to_ttml(&lrc, duration).map(|ttml| FetchedLyrics {
                    ttml,
                    kind: LyricsKind::Line,
                    quality: LyricsQuality::Line,
                    matched: None,
                })
            }
            Some(FoundLyrics::Plain(text)) => plain_to_ttml(&text).map(|ttml| FetchedLyrics {
                ttml,
                kind: LyricsKind::Line,
                quality: LyricsQuality::Plain,
                matched: None,
            }),
            Some(FoundLyrics::Instrumental) | None => None,
        })
    }
}

/// Apple Music's own lyrics: `…/syllable-lyrics` (word-timed) first, then
/// `…/lyrics`, with the captured Apple Music tokens. Without tokens or a
/// catalog id it simply has nothing.
#[derive(Debug)]
pub struct AppleLyricsProvider {
    catalog: CatalogClient,
    storage: Storage,
    storefront: OnceCell<String>,
}

impl AppleLyricsProvider {
    /// Provider reading tokens and the storefront from `storage`.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(storage: Storage) -> Result<Self, LyricsError> {
        let http = reqwest::Client::builder().timeout(APPLE_TIMEOUT).build()?;
        Ok(Self {
            catalog: CatalogClient::with_client(storage.clone(), http),
            storage,
            storefront: OnceCell::new(),
        })
    }

    /// The user's storefront: the one the proxy saw, else asked from Apple.
    async fn storefront(&self) -> Result<&str, CatalogError> {
        self.storefront
            .get_or_try_init(|| async {
                if let Ok(Some(saved)) = self.storage.get_setting(STOREFRONT_SETTING).await
                    && let Some(id) = serde_json::from_str::<Value>(&saved)
                        .ok()
                        .as_ref()
                        .and_then(|doc| doc.pointer("/data/0/id"))
                        .and_then(Value::as_str)
                {
                    return Ok(id.to_owned());
                }
                self.catalog.storefront().await
            })
            .await
            .map(String::as_str)
    }
}

/// Maps a catalog error; `None` means "this provider cannot answer" (no
/// tokens captured yet), which counts as no lyrics.
fn catalog_error(error: CatalogError) -> Option<ProviderError> {
    match error {
        CatalogError::MissingToken(_) => None,
        CatalogError::Http(error) => Some(ProviderError::from_reqwest(&error)),
        CatalogError::Status { status, .. } => Some(ProviderError::from_status(status, None)),
        other => Some(ProviderError::Fatal(other.to_string())),
    }
}

#[async_trait]
impl LyricsProvider for AppleLyricsProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Apple
    }

    fn best_quality(&self) -> LyricsQuality {
        LyricsQuality::Word
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let Some(id) = request
            .catalog_id
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        else {
            return Ok(None);
        };
        let storefront = match self.storefront().await {
            Ok(storefront) => storefront,
            Err(error) => return catalog_error(error).map_or(Ok(None), Err),
        };
        for kind in [LyricsKind::Syllable, LyricsKind::Line] {
            let path = format!(
                "/v1/catalog/{storefront}/songs/{id}/{}",
                kind.resource_type()
            );
            let response = match self.catalog.get_api(&path).await {
                Ok(response) => response,
                Err(error) => return catalog_error(error).map_or(Ok(None), Err),
            };
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            if !status.is_success() {
                return Err(ProviderError::from_status(
                    status.as_u16(),
                    retry_after(response.headers()),
                ));
            }
            let document: Value = response
                .json()
                .await
                .map_err(|e| ProviderError::from_reqwest(&e))?;
            if let Some(ttml) = apple_ttml(&document) {
                return Ok(Some(FetchedLyrics {
                    quality: ttml_quality(ttml),
                    ttml: ttml.to_owned(),
                    kind,
                    matched: None,
                }));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rate_limit_should_be_retryable() {
        assert!(ProviderError::from_status(429, None).is_retryable());
    }

    #[test]
    fn server_error_should_be_retryable() {
        assert!(ProviderError::from_status(503, None).is_retryable());
    }

    #[test]
    fn forbidden_should_be_fatal() {
        assert!(!ProviderError::from_status(403, None).is_retryable());
    }

    #[test]
    fn apple_ttml_should_read_legacy_key() {
        let doc = json!({"data": [{"attributes": {"ttml": "<tt/>"}}]});
        assert_eq!(apple_ttml(&doc), Some("<tt/>"));
    }
}
