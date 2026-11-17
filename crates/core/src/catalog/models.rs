//! Typed domain models returned by [`crate::catalog::CatalogClient`].
//!
//! These are the trimmed, strongly-typed shapes we care about — not the full
//! Apple Music JSON:API documents. Parsing lives in [`client`](super::client).

/// A song from the Apple Music catalog or the user's library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Song {
    /// Apple Music resource id (catalog numeric, or `i.*` / `a.*` for library).
    pub id: String,
    /// Song title.
    pub title: String,
    /// Primary artist name.
    pub artist: String,
    /// Album name, if present.
    pub album: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: u32,
    /// Raw artwork URL with `{w}x{h}` size placeholders, if present.
    pub artwork_url: Option<String>,
}

/// A playable catalog song from a search, with the fields used to match it
/// against another item (see [`crate::catalog::CatalogClient::search_songs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SongCandidate {
    /// Catalog (adam) id.
    pub id: String,
    /// Song title.
    pub title: String,
    /// Artist name.
    pub artist: String,
    /// Album name, if present.
    pub album: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u32>,
    /// Track number within its album.
    pub track_number: Option<u32>,
    /// ISRC.
    pub isrc: Option<String>,
}

/// An album from the Apple Music catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    /// Apple Music album id.
    pub id: String,
    /// Album name.
    pub name: String,
    /// Primary artist name.
    pub artist: String,
    /// Raw artwork URL with `{w}x{h}` size placeholders, if present.
    pub artwork_url: Option<String>,
    /// Ordered track list.
    pub tracks: Vec<Song>,
}

/// A playlist from the Apple Music catalog or the user's library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    /// Apple Music playlist id (`pl.*` for catalog, `p.*` for library).
    pub id: String,
    /// Playlist name.
    pub name: String,
    /// Curator name, if present.
    pub curator: Option<String>,
    /// Raw artwork URL with `{w}x{h}` size placeholders, if present.
    pub artwork_url: Option<String>,
    /// Ordered track list (fully paginated).
    pub tracks: Vec<Song>,
}

/// Union of the item kinds [`crate::catalog::CatalogClient::resolve`] can
/// return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogItem {
    /// An [`Album`].
    Album(Album),
    /// A [`Playlist`].
    Playlist(Playlist),
    /// A [`Song`].
    Song(Song),
}
