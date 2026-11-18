//! Normalized cache keys for HTTP requests.

use hudsucker::hyper::{Method, Uri};

/// Query parameters dropped from cache keys because they change between
/// otherwise identical requests (cache busters / timestamps). Matched
/// case-insensitively.
pub const VOLATILE_QUERY_PARAMS: &[&str] = &["_", "t", "ts", "timestamp", "cb", "cachebust"];

/// Builds a normalized cache key: `METHOD scheme://host[:port]/path?sorted-query`.
///
/// - method upper-case, scheme and host lower-case, default ports dropped;
/// - query pairs sorted (by name, then value), empty pairs and
///   [`VOLATILE_QUERY_PARAMS`] removed; percent-encoding is kept verbatim.
///
/// Returns `None` if the URI has no host (origin-form requests).
pub fn cache_key(method: &Method, uri: &Uri) -> Option<String> {
    let host = uri.host()?.to_ascii_lowercase();
    let scheme = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };

    let mut key = format!("{} {scheme}://{host}", method.as_str().to_ascii_uppercase());
    if let Some(port) = uri.port_u16().filter(|p| Some(*p) != default_port) {
        key.push(':');
        key.push_str(&port.to_string());
    }
    key.push_str(match uri.path() {
        "" => "/",
        path => path,
    });

    let query = normalize_query(uri.query().unwrap_or_default());
    if !query.is_empty() {
        key.push('?');
        key.push_str(&query);
    }
    Some(key)
}

/// Sorts query pairs and removes volatile and empty ones.
pub fn normalize_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .filter(|(name, _)| {
            !name.is_empty()
                && !VOLATILE_QUERY_PARAMS
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(name))
        })
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(name, value)| {
            if value.is_empty() {
                (*name).to_owned()
            } else {
                format!("{name}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(method: &Method, uri: &str) -> String {
        cache_key(method, &uri.parse().unwrap()).unwrap()
    }

    #[test]
    fn should_sort_query_parameters() {
        assert_eq!(
            key(
                &Method::GET,
                "https://amp-api.music.apple.com/v1/catalog/us/songs?l=en-US&ids=1,2"
            ),
            "GET https://amp-api.music.apple.com/v1/catalog/us/songs?ids=1,2&l=en-US"
        );
    }

    #[test]
    fn should_drop_volatile_parameters() {
        assert_eq!(
            key(
                &Method::GET,
                "https://amp-api.music.apple.com/v1/x?a=1&_=12345&TS=9"
            ),
            "GET https://amp-api.music.apple.com/v1/x?a=1"
        );
    }

    #[test]
    fn should_lowercase_host_and_drop_default_port() {
        assert_eq!(
            key(&Method::GET, "https://AMP-API.Music.Apple.com:443/v1/x"),
            "GET https://amp-api.music.apple.com/v1/x"
        );
    }

    #[test]
    fn should_keep_non_default_port() {
        assert_eq!(
            key(&Method::GET, "http://localhost:8080/a"),
            "GET http://localhost:8080/a"
        );
    }

    #[test]
    fn should_treat_parameter_order_as_equivalent() {
        assert_eq!(
            key(&Method::GET, "https://h.example/p?b=2&a=1"),
            key(&Method::GET, "https://h.example/p?a=1&b=2")
        );
    }

    #[test]
    fn should_distinguish_methods() {
        assert_ne!(
            key(&Method::GET, "https://h.example/p"),
            key(&Method::POST, "https://h.example/p")
        );
    }

    #[test]
    fn should_omit_empty_query() {
        assert_eq!(
            key(&Method::GET, "https://h.example/p?&_=1"),
            "GET https://h.example/p"
        );
    }

    #[test]
    fn should_return_none_without_host() {
        assert!(cache_key(&Method::GET, &"/relative".parse().unwrap()).is_none());
    }
}
