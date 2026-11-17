//! Classification of Apple Music endpoints (hosts and paths) used by the
//! built-in interceptors.
//!
//! The endpoint shapes are what Cider / `MusicKit` JS were observed to use;
//! anything not recognized is simply passed through.

use hudsucker::hyper::Uri;

/// Returns whether `host` serves the Apple Music JSON API
/// (`amp-api.music.apple.com`, `amp-api-edge.music.apple.com`, `api.music.apple.com`).
pub(crate) fn is_api_host(host: &str) -> bool {
    let host = normalize_host(host);
    host == "api.music.apple.com"
        || (host.starts_with("amp-api") && host.ends_with(".music.apple.com"))
}

/// Returns whether `host` is the artwork CDN (`*.mzstatic.com`).
pub(crate) fn is_artwork_host(host: &str) -> bool {
    let host = normalize_host(host);
    host == "mzstatic.com" || host.ends_with(".mzstatic.com")
}

/// Returns whether `host` serves signed user-uploaded artwork
/// (`*.blobstore.apple.com`, e.g. custom playlist covers).
pub(crate) fn is_blobstore_host(host: &str) -> bool {
    normalize_host(host).ends_with(".blobstore.apple.com")
}

/// Returns whether `host` receives play-activity reports
/// (`universal-activity-service.itunes.apple.com`).
pub(crate) fn is_activity_host(host: &str) -> bool {
    normalize_host(host) == "universal-activity-service.itunes.apple.com"
}

/// Returns whether `host` is the playback service (`play.itunes.apple.com`).
pub(crate) fn is_playback_host(host: &str) -> bool {
    normalize_host(host) == "play.itunes.apple.com"
}

fn normalize_host(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// Non-empty path segments.
pub(crate) fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

/// Returns whether the path belongs to an auth / token / license / playback
/// endpoint, which must never be cached or synthesized from cache.
pub(crate) fn is_sensitive_path(path: &str) -> bool {
    segments(path).any(|segment| {
        let segment = segment.to_ascii_lowercase();
        segment == "play"
            || segment == "playback"
            || segment.contains("webplayback")
            || segment.contains("license")
            || segment.contains("token")
            || segment.contains("auth")
            || segment.contains("login")
            || segment.contains("signup")
    })
}

/// Returns whether the path is the `WebPlayback` asset request
/// (`/WebObjects/MZPlay.woa/wa/webPlayback`), excluding license acquisition.
pub(crate) fn is_web_playback_path(path: &str) -> bool {
    segments(path)
        .last()
        .is_some_and(|last| last.eq_ignore_ascii_case("webPlayback"))
}

/// Returns whether the path is `/v1/me/storefront`.
pub(crate) fn is_my_storefront_path(path: &str) -> bool {
    let parts: Vec<&str> = segments(path).collect();
    parts == ["v1", "me", "storefront"]
}

/// Which lyrics flavour a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LyricsKind {
    /// `…/songs/{id}/lyrics`: line-timed TTML.
    Line,
    /// `…/songs/{id}/syllable-lyrics`: word-timed TTML.
    Syllable,
}

impl LyricsKind {
    /// JSON `type` of the lyrics resource.
    pub(crate) fn resource_type(self) -> &'static str {
        match self {
            Self::Line => "lyrics",
            Self::Syllable => "syllable-lyrics",
        }
    }
}

/// Parses `…/songs/{id}/lyrics` and `…/songs/{id}/syllable-lyrics`.
pub(crate) fn lyrics_target(path: &str) -> Option<(String, LyricsKind)> {
    let parts: Vec<&str> = segments(path).collect();
    parts.windows(3).find_map(|w| {
        if w[0] != "songs" {
            return None;
        }
        let kind = match w[2] {
            "lyrics" => LyricsKind::Line,
            "syllable-lyrics" => LyricsKind::Syllable,
            _ => return None,
        };
        Some((w[1].to_owned(), kind))
    })
}

/// Which search endpoint a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchKind {
    /// `/v1/catalog/{storefront}/search`: results under `results.song`.
    Catalog,
    /// `/v1/me/library/search`: results under `results["library-songs"]`.
    Library,
}

impl SearchKind {
    /// Key of the song results group inside `results`.
    ///
    /// Real Apple `catalog` responses key this group by the singular `"song"`
    /// (the item objects inside stay `type: "songs"` — see [`Self::song_resource_type`]).
    pub(crate) fn songs_key(self) -> &'static str {
        match self {
            Self::Catalog => "song",
            // Unverified against real traffic; kept as the library group key Cider uses.
            Self::Library => "library-songs",
        }
    }

    /// `type` of the individual song resources inside the group (always plural).
    pub(crate) fn song_resource_type(self) -> &'static str {
        match self {
            Self::Catalog => "songs",
            Self::Library => "library-songs",
        }
    }
}

/// Parses the catalog / library search paths.
pub(crate) fn search_kind(path: &str) -> Option<SearchKind> {
    let parts: Vec<&str> = segments(path).collect();
    match parts.as_slice() {
        ["v1", "catalog", _, "search"] => Some(SearchKind::Catalog),
        ["v1", "me", "library", "search"] => Some(SearchKind::Library),
        _ => None,
    }
}

/// A single-resource or `?ids=` lookup of a catalog / library resource type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceLookup {
    /// Resource type segment (`songs`, `albums`, `playlists`).
    pub(crate) kind: String,
    /// Requested ids.
    pub(crate) ids: Vec<String>,
}

/// Parses `/v1/catalog/{sf}/{kind}/{id}`, `/v1/me/library/{kind}/{id}` and the
/// `?ids=a,b` list forms.
pub(crate) fn resource_lookup(uri: &Uri) -> Option<ResourceLookup> {
    let parts: Vec<&str> = segments(uri.path()).collect();
    let (kind, id) = match parts.as_slice() {
        ["v1", "catalog", _, kind, id] | ["v1", "me", "library", kind, id] => (*kind, Some(*id)),
        ["v1", "catalog", _, kind] | ["v1", "me", "library", kind] => (*kind, None),
        _ => return None,
    };
    if !matches!(kind, "songs" | "albums" | "playlists") {
        return None;
    }
    let ids = match id {
        Some(id) => vec![id.to_owned()],
        None => query_param(uri, "ids")?
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    (!ids.is_empty()).then(|| ResourceLookup {
        kind: kind.to_owned(),
        ids,
    })
}

/// Where a response's proven library members are found, when its path is a
/// saved-library membership endpoint. Used to tag `in_library` on exactly the
/// songs the user has actually saved (never songs merely nested in a playlist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibraryMembership {
    /// The top-level `data[]` song resources are saved library members:
    /// `/v1/me/library/songs` (the saved-songs list) and
    /// `/v1/me/library/albums/{id}/tracks` (a saved album's track page).
    TopLevelData,
    /// The saved album at `data[0]` carries its members under
    /// `relationships.tracks.data[]`: `/v1/me/library/albums/{id}`.
    AlbumTracks,
}

/// Classifies a path as a saved-library membership endpoint, if it is one.
///
/// Only these paths prove membership. `/v1/me/library/playlists/{id}/tracks`,
/// catalog and search paths deliberately return `None` — songs reached that way
/// are not saved-library members (they merely appear inside a playlist or a
/// browse response). Query parameters (`offset`, `limit`, `l`) are in the query
/// string, not the path, so pagination and locale variants match unchanged.
pub(crate) fn library_membership(path: &str) -> Option<LibraryMembership> {
    let parts: Vec<&str> = segments(path).collect();
    match parts.as_slice() {
        ["v1", "me", "library", "songs"] | ["v1", "me", "library", "albums", _, "tracks"] => {
            Some(LibraryMembership::TopLevelData)
        }
        ["v1", "me", "library", "albums", _] => Some(LibraryMembership::AlbumTracks),
        _ => None,
    }
}

/// Parses `…/{playlists|albums}/{id}/tracks` (a collection's track page).
pub(crate) fn collection_tracks_target(path: &str) -> Option<String> {
    let parts: Vec<&str> = segments(path).collect();
    match parts.as_slice() {
        [.., "playlists" | "albums", id, "tracks"] => Some((*id).to_owned()),
        _ => None,
    }
}

/// First value of a (percent-decoded) query parameter.
pub(crate) fn query_param(uri: &Uri, name: &str) -> Option<String> {
    let query = uri.query()?;
    // Reuse the `url` crate's form decoding through reqwest's re-export.
    let url = reqwest::Url::parse(&format!("http://q/?{query}")).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn amp_api_should_be_api_host() {
        assert!(is_api_host("amp-api.music.apple.com"));
    }

    #[test]
    fn edge_api_should_be_api_host() {
        assert!(is_api_host("amp-api-edge.music.apple.com"));
    }

    #[test]
    fn web_player_host_should_not_be_api_host() {
        assert!(!is_api_host("beta.music.apple.com"));
    }

    #[test]
    fn artwork_cdn_should_match() {
        assert!(is_artwork_host("is1-ssl.mzstatic.com"));
    }

    #[test]
    fn license_path_should_be_sensitive() {
        assert!(is_sensitive_path(
            "/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense"
        ));
    }

    #[test]
    fn playlists_path_should_not_be_sensitive() {
        assert!(!is_sensitive_path("/v1/me/library/playlists/p.1"));
    }

    #[test]
    fn web_playback_should_not_match_license_request() {
        assert!(!is_web_playback_path(
            "/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense"
        ));
    }

    #[test]
    fn web_playback_should_match() {
        assert!(is_web_playback_path(
            "/WebObjects/MZPlay.woa/wa/webPlayback"
        ));
    }

    #[test]
    fn syllable_lyrics_target_should_parse() {
        assert_eq!(
            lyrics_target("/v1/catalog/us/songs/123/syllable-lyrics"),
            Some(("123".to_owned(), LyricsKind::Syllable))
        );
    }

    #[test]
    fn song_path_should_not_be_lyrics() {
        assert_eq!(lyrics_target("/v1/catalog/us/songs/123"), None);
    }

    #[test]
    fn library_search_should_parse() {
        assert_eq!(
            search_kind("/v1/me/library/search"),
            Some(SearchKind::Library)
        );
    }

    #[test]
    fn ids_lookup_should_split_ids() {
        assert_eq!(
            resource_lookup(&uri(
                "https://amp-api.music.apple.com/v1/catalog/us/songs?ids=1,2&l=en"
            )),
            Some(ResourceLookup {
                kind: "songs".to_owned(),
                ids: vec!["1".to_owned(), "2".to_owned()]
            })
        );
    }

    #[test]
    fn single_lookup_should_parse_library_album() {
        assert_eq!(
            resource_lookup(&uri(
                "https://amp-api.music.apple.com/v1/me/library/albums/l.x"
            ))
            .map(|l| l.kind),
            Some("albums".to_owned())
        );
    }

    #[test]
    fn query_param_should_decode() {
        assert_eq!(
            query_param(&uri("https://h/s?term=hello+w%C3%B6rld"), "term").as_deref(),
            Some("hello wörld")
        );
    }

    #[test]
    fn collection_tracks_should_parse() {
        assert_eq!(
            collection_tracks_target("/v1/me/library/playlists/p.abc/tracks").as_deref(),
            Some("p.abc")
        );
    }

    #[test]
    fn library_songs_list_should_be_top_level_membership() {
        assert_eq!(
            library_membership("/v1/me/library/songs"),
            Some(LibraryMembership::TopLevelData)
        );
    }

    #[test]
    fn library_album_tracks_should_be_top_level_membership() {
        assert_eq!(
            library_membership("/v1/me/library/albums/l.x/tracks"),
            Some(LibraryMembership::TopLevelData)
        );
    }

    #[test]
    fn library_album_should_be_album_tracks_membership() {
        assert_eq!(
            library_membership("/v1/me/library/albums/l.x"),
            Some(LibraryMembership::AlbumTracks)
        );
    }

    #[test]
    fn playlist_tracks_should_not_be_membership() {
        assert_eq!(
            library_membership("/v1/me/library/playlists/p.1/tracks"),
            None
        );
    }

    #[test]
    fn catalog_songs_should_not_be_membership() {
        assert_eq!(library_membership("/v1/catalog/us/songs/1"), None);
    }
}
