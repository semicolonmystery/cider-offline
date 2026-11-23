//! Row types of the repository API.

use std::path::{Path, PathBuf};

/// Id prefix shared by every custom (user-created) item. Cheap to detect in
/// interceptors and playback (`id.starts_with(CUSTOM_ID_PREFIX)`).
pub const CUSTOM_ID_PREFIX: &str = "local-";
/// Id prefix of a custom song (`local-<n>`).
pub const CUSTOM_SONG_ID_PREFIX: &str = "local-";
/// Id prefix of a custom album (`local-al-<n>`).
pub const CUSTOM_ALBUM_ID_PREFIX: &str = "local-al-";
/// Id prefix of a custom playlist (`local-pl-<n>`).
pub const CUSTOM_PLAYLIST_ID_PREFIX: &str = "local-pl-";

/// A cached HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// Normalized cache key (see [`crate::proxy::cache_key`]).
    pub key: String,
    /// Request method.
    pub method: String,
    /// Original request URL.
    pub url: String,
    /// Response status code.
    pub status: u16,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Decoded response body.
    pub body: Vec<u8>,
    /// Unix seconds when stored.
    pub stored_at: i64,
}

/// Where a track's lyrics come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LyricsSource {
    /// No lyrics available.
    #[default]
    None,
    /// Lyrics cached from Apple Music.
    Apple,
    /// Lyrics fetched from LRCLIB.
    Lrclib,
    /// Lyrics from the AMLL TTML database.
    Amll,
    /// Lyrics from `NetEase` Cloud Music.
    Netease,
    /// Lyrics from Kugou.
    Kugou,
    /// Lyrics from Musixmatch (through Cider's backend).
    Musixmatch,
    /// Lyrics contributed by Cider users.
    Community,
    /// Lyrics the user imported from a file. They always win over every
    /// other source, online and offline, and nothing overwrites them.
    Custom,
}

impl LyricsSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Apple => "apple",
            Self::Lrclib => "lrclib",
            Self::Amll => "amll",
            Self::Netease => "netease",
            Self::Kugou => "kugou",
            Self::Musixmatch => "musixmatch",
            Self::Community => "community",
            Self::Custom => "custom",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "apple" => Some(Self::Apple),
            "lrclib" => Some(Self::Lrclib),
            "amll" => Some(Self::Amll),
            "netease" => Some(Self::Netease),
            "kugou" => Some(Self::Kugou),
            "musixmatch" => Some(Self::Musixmatch),
            "community" => Some(Self::Community),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

/// How finely the local lyrics are timed. Ordered: `Word` is the best.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LyricsQuality {
    /// No lyrics.
    #[default]
    None,
    /// Text without timing.
    Plain,
    /// Timed per line.
    Line,
    /// Timed per word or syllable (karaoke).
    Word,
}

impl LyricsQuality {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Plain => "plain",
            Self::Line => "line",
            Self::Word => "word",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "plain" => Some(Self::Plain),
            "line" => Some(Self::Line),
            "word" => Some(Self::Word),
            _ => None,
        }
    }

    /// Whether the lyrics carry timestamps (line or word).
    pub fn is_synced(self) -> bool {
        self >= Self::Line
    }
}

/// How a custom song's audio is provided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CustomAudioMode {
    /// A file copied into the media directory (the [`AudioVariant::Stereo`]
    /// file in [`Track::audio`]).
    LocalCopy,
    /// Audio fetched via the downloader for a referenced catalog id (in
    /// [`Track::reference_track_id`]).
    Reference,
}

impl CustomAudioMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LocalCopy => "local",
            Self::Reference => "reference",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Self::LocalCopy),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

/// A track known to the local library.
///
/// The same type carries real cached songs and user-created custom songs
/// (`is_custom == true`, with an `id` of `local-<n>`). The rich attribute
/// columns are optional so real tracks observed with little detail and custom
/// songs built field-by-field both fit.
#[derive(Debug, Clone, PartialEq, Eq)]
// Each bool is an independent, orthogonal attribute of a song (lyrics
// availability, time-synced lyrics, custom-origin, library membership); they are
// not a state machine, so a struct of flags is the right shape.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent song attributes, not a combinable state"
)]
pub struct Track {
    /// Apple Music id, or `local-<n>` for a custom song.
    pub id: String,
    /// Catalog id of a library track (`playParams.catalogId`), when this row is
    /// a library (`i.…`) track whose catalog twin is known. `None` for a
    /// catalog-native row or a custom song. The downloader keys on this so a
    /// library track can be fetched by its catalog (adam) id.
    pub catalog_id: Option<String>,
    /// Title.
    pub title: String,
    /// Artist name.
    pub artist: String,
    /// Album name.
    pub album: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u32>,
    /// Artwork URL template.
    pub artwork_url: Option<String>,
    /// Raw / synthesized JSON resource, if kept.
    pub metadata: Option<String>,
    /// Album artist name.
    pub album_artist: Option<String>,
    /// Composer name.
    pub composer: Option<String>,
    /// Primary genre (full list stays in [`Track::metadata`]).
    pub genre: Option<String>,
    /// 1-based track number within its album.
    pub track_number: Option<u32>,
    /// Disc number.
    pub disc_number: Option<u32>,
    /// Release date (ISO string).
    pub release_date: Option<String>,
    /// ISRC.
    pub isrc: Option<String>,
    /// Canonical music.apple.com URL.
    pub url: Option<String>,
    /// Whether lyrics exist for this track.
    pub has_lyrics: bool,
    /// Whether time-synced (word/line) lyrics exist.
    pub has_time_synced_lyrics: bool,
    /// Content rating (e.g. `"explicit"`).
    pub content_rating: Option<String>,
    /// Whether this is a user-created custom song.
    pub is_custom: bool,
    /// Real album id this custom song is tagged into, if any.
    pub real_album_id: Option<String>,
    /// How a custom song's audio is provided, if set.
    pub audio_mode: Option<CustomAudioMode>,
    /// Referenced catalog id when [`Track::audio_mode`] is
    /// [`CustomAudioMode::Reference`].
    pub reference_track_id: Option<String>,
    /// Downloaded audio files, one per variant (the `track_audio` rows).
    pub audio: TrackAudio,
    /// Lyrics availability.
    pub lyrics_source: LyricsSource,
    /// How finely the local lyrics are timed (`None` without lyrics).
    pub lyrics_quality: LyricsQuality,
    /// Whether this song was observed as a genuinely saved member of the user's
    /// Apple Music library (the `/v1/me/library/songs` list or a saved library
    /// album's tracks), as opposed to merely appearing inside a playlist or a
    /// catalog / search response. Sticky once set. See the `in_library` column.
    pub in_library: bool,
}

impl Track {
    /// A minimal real (non-custom) track with only the core fields set. Rich
    /// columns default to empty; convenient for callers and tests that only
    /// carry title/artist/album/duration/artwork.
    #[must_use]
    pub fn minimal(
        id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            catalog_id: None,
            title: title.into(),
            artist: artist.into(),
            album: None,
            duration_ms: None,
            artwork_url: None,
            metadata: None,
            album_artist: None,
            composer: None,
            genre: None,
            track_number: None,
            disc_number: None,
            release_date: None,
            isrc: None,
            url: None,
            has_lyrics: false,
            has_time_synced_lyrics: false,
            content_rating: None,
            is_custom: false,
            real_album_id: None,
            audio_mode: None,
            reference_track_id: None,
            audio: TrackAudio::default(),
            lyrics_source: LyricsSource::None,
            lyrics_quality: LyricsQuality::None,
            in_library: false,
        }
    }
}

impl Track {
    /// Whether any audio variant is recorded for this row ("downloaded").
    /// File existence is not checked.
    pub fn has_audio(&self) -> bool {
        !self.audio.is_empty()
    }
}

/// Which downloaded version of a song an audio file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AudioVariant {
    /// The stereo stream Cider plays by default (`ctrp256`); also a custom
    /// song's copied local file.
    Stereo,
    /// The Dolby Atmos mix rendered for headphones (binaural stereo).
    Spatial,
}

impl AudioVariant {
    /// Every variant, in preference order for playback fallback.
    pub const ALL: [Self; 2] = [Self::Stereo, Self::Spatial];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stereo => "stereo",
            Self::Spatial => "spatial",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "stereo" => Some(Self::Stereo),
            "spatial" => Some(Self::Spatial),
            _ => None,
        }
    }
}

impl std::fmt::Display for AudioVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The downloaded audio files of one track row, by variant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackAudio {
    /// The [`AudioVariant::Stereo`] file.
    pub normal: Option<PathBuf>,
    /// The [`AudioVariant::Spatial`] file.
    pub spatial: Option<PathBuf>,
}

impl TrackAudio {
    /// Only a normal file.
    #[must_use]
    pub fn normal(path: impl Into<PathBuf>) -> Self {
        Self {
            normal: Some(path.into()),
            spatial: None,
        }
    }

    /// The file of `variant`, if recorded.
    pub fn get(&self, variant: AudioVariant) -> Option<&Path> {
        match variant {
            AudioVariant::Stereo => self.normal.as_deref(),
            AudioVariant::Spatial => self.spatial.as_deref(),
        }
    }

    /// Records (or clears with `None`) the file of `variant`.
    pub fn set(&mut self, variant: AudioVariant, path: Option<PathBuf>) {
        match variant {
            AudioVariant::Stereo => self.normal = path,
            AudioVariant::Spatial => self.spatial = path,
        }
    }

    /// Whether no variant is recorded.
    pub fn is_empty(&self) -> bool {
        self.normal.is_none() && self.spatial.is_none()
    }

    /// The recorded variants and their files, normal first.
    pub fn iter(&self) -> impl Iterator<Item = (AudioVariant, &Path)> {
        AudioVariant::ALL
            .into_iter()
            .filter_map(|variant| self.get(variant).map(|path| (variant, path)))
    }

    /// The file to play when the caller has no variant preference: normal,
    /// else spatial.
    pub fn preferred(&self) -> Option<&Path> {
        self.iter().next().map(|(_, path)| path)
    }
}

/// One downloaded audio file of a track, with what is known about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFile {
    /// Which version of the song the file holds.
    pub variant: AudioVariant,
    /// The file (absolute when read back).
    pub path: PathBuf,
    /// File size in bytes, if known.
    pub size: Option<u64>,
    /// Playback duration in milliseconds, if known.
    pub duration_ms: Option<u32>,
}

impl AudioFile {
    /// A file of `variant` with no size or duration recorded.
    #[must_use]
    pub fn new(variant: AudioVariant, path: impl Into<PathBuf>) -> Self {
        Self {
            variant,
            path: path.into(),
            size: None,
            duration_ms: None,
        }
    }
}

/// Whether a [`Playlist`] row is a playlist or an album.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollectionKind {
    /// A playlist.
    Playlist,
    /// An album.
    Album,
}

impl CollectionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Album => "album",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "playlist" => Some(Self::Playlist),
            "album" => Some(Self::Album),
            _ => None,
        }
    }

    /// Id prefix for a custom collection of this kind.
    pub(crate) fn custom_id_prefix(self) -> &'static str {
        match self {
            Self::Playlist => CUSTOM_PLAYLIST_ID_PREFIX,
            Self::Album => CUSTOM_ALBUM_ID_PREFIX,
        }
    }

    /// `id_seq` counter name for a custom collection of this kind.
    pub(crate) fn seq_name(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Album => "album",
        }
    }
}

/// A cached playlist or album. Custom collections set `is_custom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    /// Apple Music id, or `local-al-<n>` / `local-pl-<n>` for a custom one.
    pub id: String,
    /// Playlist or album.
    pub kind: CollectionKind,
    /// Display name.
    pub name: String,
    /// Curator / album artist.
    pub artist: Option<String>,
    /// Artwork URL template.
    pub artwork_url: Option<String>,
    /// Raw / synthesized JSON resource, if kept.
    pub metadata: Option<String>,
    /// Whether this is a user-created custom album / playlist.
    pub is_custom: bool,
    /// Description / editorial notes.
    pub description: Option<String>,
    /// Primary genre.
    pub genre: Option<String>,
    /// Release date (ISO string).
    pub release_date: Option<String>,
}

impl Playlist {
    /// A minimal real (non-custom) collection with only the core fields set.
    #[must_use]
    pub fn minimal(id: impl Into<String>, kind: CollectionKind, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            name: name.into(),
            artist: None,
            artwork_url: None,
            metadata: None,
            is_custom: false,
            description: None,
            genre: None,
            release_date: None,
        }
    }
}

/// Fields for creating or editing a custom album / playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCustomCollection {
    /// Album or playlist.
    pub kind: CollectionKind,
    /// Display name.
    pub name: String,
    /// Album artist / curator.
    pub artist: Option<String>,
    /// Artwork URL template or local artwork reference.
    pub artwork_url: Option<String>,
    /// Description / editorial notes.
    pub description: Option<String>,
    /// Primary genre.
    pub genre: Option<String>,
    /// Release date (ISO string).
    pub release_date: Option<String>,
    /// Pre-synthesized Apple-shaped resource JSON, if any.
    pub metadata: Option<String>,
}

/// How a custom song's audio is supplied when creating or editing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CustomAudio {
    /// No audio yet.
    #[default]
    None,
    /// A file already copied into the media directory.
    LocalCopy(PathBuf),
    /// Audio fetched via the downloader for this referenced catalog id.
    Reference(String),
}

/// Full attribute set for creating or editing a custom song. Mirrors the real
/// `/v1/catalog/{sf}/songs/{id}` attributes so a custom song can be as rich as a
/// real one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewCustomTrack {
    /// Title (`name`).
    pub title: String,
    /// Artist (`artistName`).
    pub artist: String,
    /// Album name (`albumName`).
    pub album: Option<String>,
    /// Album artist (`albumArtistName`).
    pub album_artist: Option<String>,
    /// Composer (`composerName`).
    pub composer: Option<String>,
    /// Primary genre.
    pub genre: Option<String>,
    /// Track number within its album.
    pub track_number: Option<u32>,
    /// Disc number.
    pub disc_number: Option<u32>,
    /// Release date (ISO string).
    pub release_date: Option<String>,
    /// ISRC.
    pub isrc: Option<String>,
    /// Canonical URL.
    pub url: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u32>,
    /// Artwork URL template or local artwork reference.
    pub artwork_url: Option<String>,
    /// Whether lyrics are available.
    pub has_lyrics: bool,
    /// Whether time-synced lyrics are available.
    pub has_time_synced_lyrics: bool,
    /// Content rating.
    pub content_rating: Option<String>,
    /// Real album id to tag this song into (borrows its identity), if any.
    pub real_album_id: Option<String>,
    /// How the audio is supplied.
    pub audio: CustomAudio,
    /// Pre-synthesized Apple-shaped resource JSON, if any.
    pub metadata: Option<String>,
}

/// Maps an Apple Music track id to a local audio file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMapping {
    /// Apple Music track id.
    pub track_id: String,
    /// Local audio file.
    pub file_path: PathBuf,
}

/// Kinds of captured auth material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TokenKind {
    /// The `Authorization: Bearer …` developer token.
    DeveloperToken,
    /// The `media-user-token` / `music-user-token`.
    MediaUserToken,
}

impl TokenKind {
    /// Storage key.
    pub fn key(self) -> &'static str {
        match self {
            Self::DeveloperToken => "developer_token",
            Self::MediaUserToken => "media_user_token",
        }
    }
}
