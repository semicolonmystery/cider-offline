//! Captures Apple Music auth tokens from outgoing requests into storage.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use async_trait::async_trait;
use hudsucker::Body;
use hudsucker::hyper::header::{AUTHORIZATION, COOKIE};
use hudsucker::hyper::{HeaderMap, Request};

use super::interceptor::{Exchange, Interceptor, RequestOutcome};
use crate::library_sync::LibrarySync;
use crate::storage::TokenKind;

const MEDIA_USER_TOKEN_NAMES: &[&str] = &["media-user-token", "music-user-token"];

/// Stores the developer token (`Authorization`) and the media-user token
/// (header or cookie) whenever they change. Never logs values. With a
/// [`LibrarySync`] attached, each stored change triggers a library sync.
#[derive(Debug, Default)]
pub struct TokenCaptureInterceptor {
    /// Fingerprints of the last stored values, to skip redundant writes.
    /// Hashes rather than values so secrets are not kept twice in memory.
    last_stored: Mutex<HashMap<TokenKind, u64>>,
    /// Notified after a token was stored.
    library_sync: Option<LibrarySync>,
}

impl TokenCaptureInterceptor {
    /// Creates the interceptor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Triggers `sync` whenever a captured token was stored.
    #[must_use]
    pub fn with_library_sync(mut self, sync: LibrarySync) -> Self {
        self.library_sync = Some(sync);
        self
    }

    /// Returns `true` if `value` differs from the last stored value of `kind`
    /// and records it as stored.
    fn changed(&self, kind: TokenKind, value: &str) -> bool {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        let fingerprint = hasher.finish();
        // A poisoned lock only means another thread panicked mid-insert; the
        // map is still usable as a best-effort dedup cache.
        let mut last = self
            .last_stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.insert(kind, fingerprint) != Some(fingerprint)
    }
}

/// Extracts tokens from request headers.
pub(crate) fn extract_tokens(headers: &HeaderMap) -> Vec<(TokenKind, String)> {
    let mut tokens = Vec::with_capacity(2);
    if let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
    {
        tokens.push((TokenKind::DeveloperToken, value.to_owned()));
    }

    let from_header = MEDIA_USER_TOKEN_NAMES.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    });
    let media_user = from_header.or_else(|| {
        headers
            .get_all(COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|cookies| cookies.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .find(|(name, value)| {
                !value.is_empty() && MEDIA_USER_TOKEN_NAMES.iter().any(|n| n == name)
            })
            .map(|(_, value)| value.to_owned())
    });
    if let Some(value) = media_user {
        tokens.push((TokenKind::MediaUserToken, value));
    }
    tokens
}

#[async_trait]
impl Interceptor for TokenCaptureInterceptor {
    fn name(&self) -> &'static str {
        "token-capture"
    }

    async fn on_request(&self, exchange: &mut Exchange, request: Request<Body>) -> RequestOutcome {
        for (kind, value) in extract_tokens(request.headers()) {
            if !self.changed(kind, &value) {
                continue;
            }
            let storage = exchange.storage().clone();
            let sync = self.library_sync.clone();
            exchange.spawn(async move {
                match storage.set_auth_token(kind, value).await {
                    Ok(()) => {
                        tracing::info!(token = kind.key(), "captured auth token");
                        if let Some(sync) = sync {
                            sync.tokens_changed();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(token = kind.key(), %error, "failed to store auth token");
                    }
                }
            });
        }
        RequestOutcome::Continue(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hudsucker::hyper::header::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn should_extract_authorization_and_header_token() {
        let tokens = extract_tokens(&headers(&[
            ("authorization", "Bearer dev"),
            ("media-user-token", "mut"),
        ]));
        assert_eq!(
            tokens,
            [
                (TokenKind::DeveloperToken, "Bearer dev".to_owned()),
                (TokenKind::MediaUserToken, "mut".to_owned())
            ]
        );
    }

    #[test]
    fn should_extract_media_user_token_from_cookie() {
        let tokens = extract_tokens(&headers(&[("cookie", "a=1; music-user-token=abc; b=2")]));
        assert_eq!(tokens, [(TokenKind::MediaUserToken, "abc".to_owned())]);
    }

    #[test]
    fn should_extract_nothing_without_tokens() {
        assert_eq!(
            extract_tokens(&headers(&[("accept", "*/*")])),
            Vec::<(TokenKind, String)>::new()
        );
    }

    #[test]
    fn changed_should_dedupe_identical_values() {
        let interceptor = TokenCaptureInterceptor::new();
        assert!(interceptor.changed(TokenKind::DeveloperToken, "x"));
        assert!(!interceptor.changed(TokenKind::DeveloperToken, "x"));
    }
}
