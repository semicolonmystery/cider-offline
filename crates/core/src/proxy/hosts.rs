//! Classification of Apple Music hosts.

/// Domains whose hosts (the domain itself and any subdomain) are treated as
/// Apple Music traffic in [`crate::Mode::Active`]:
///
/// - `music.apple.com`: `amp-api.music.apple.com` (catalog/library/lyrics API) and friends
/// - `itunes.apple.com`: `play.itunes.apple.com` (playback) and related services
/// - `mzstatic.com`: artwork CDN (`is1-ssl.mzstatic.com`, …)
pub const APPLE_MUSIC_DOMAINS: &[&str] = &["music.apple.com", "itunes.apple.com", "mzstatic.com"];

/// Returns whether `host` (no port, any case, optional trailing dot) belongs
/// to one of [`APPLE_MUSIC_DOMAINS`].
pub fn is_apple_music_host(host: &str) -> bool {
    // Byte-wise comparison: hosts may contain non-ASCII, and slicing a `str`
    // at an arbitrary byte offset could split a character.
    let host = host.strip_suffix('.').unwrap_or(host).as_bytes();
    APPLE_MUSIC_DOMAINS.iter().any(|domain| {
        let domain = domain.as_bytes();
        match host.len().checked_sub(domain.len()) {
            Some(0) => host.eq_ignore_ascii_case(domain),
            Some(prefix) => host[prefix - 1] == b'.' && host[prefix..].eq_ignore_ascii_case(domain),
            None => false,
        }
    })
}

/// Domain whose hosts are intercepted **only while offline**: signed
/// user-uploaded artwork (custom playlist covers on `*.blobstore.apple.com`).
/// Online they stay raw tunnels, exactly as before; offline the responder
/// serves the copy the library sync cached.
pub const OFFLINE_ONLY_DOMAIN: &str = "blobstore.apple.com";

/// Returns whether `host` is intercepted only while offline (see
/// [`OFFLINE_ONLY_DOMAIN`]).
pub fn is_offline_only_host(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    host.ends_with(&format!(".{OFFLINE_ONLY_DOMAIN}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_match_api_host() {
        assert!(is_apple_music_host("amp-api.music.apple.com"));
    }

    #[test]
    fn should_match_case_insensitively_with_trailing_dot() {
        assert!(is_apple_music_host("Play.iTunes.Apple.com."));
    }

    #[test]
    fn should_match_artwork_cdn() {
        assert!(is_apple_music_host("is1-ssl.mzstatic.com"));
    }

    #[test]
    fn should_not_match_lookalike_suffix() {
        assert!(!is_apple_music_host("evilmzstatic.com"));
    }

    #[test]
    fn blobstore_should_be_offline_only() {
        assert!(is_offline_only_host("store-035.blobstore.apple.com"));
    }

    #[test]
    fn should_not_match_unrelated_apple_host() {
        assert!(!is_apple_music_host("www.apple.com"));
    }
}
