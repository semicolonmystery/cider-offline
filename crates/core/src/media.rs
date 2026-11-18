//! Local audio: resolving a track to a file, the proxy-served media URL, and
//! streaming files with HTTP `Range` support.
//!
//! Local media is served under a reserved path on the playback host,
//! [`MEDIA_URL_PREFIX`]. Cider reaches it through this proxy like any other
//! Apple request, so it needs no extra port, certificate or CORS origin, and
//! it never leaves the machine: the media interceptor answers it before any
//! upstream contact (online or offline).

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use hudsucker::Body;
use hudsucker::hyper::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, RANGE,
};
use hudsucker::hyper::{HeaderMap, Method, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::respond::{LOCAL_HEADER, add_cors};
use crate::storage::{CustomAudioMode, Storage, StorageError, TrackRecord};

/// Host serving local media (the playback host, which is always intercepted).
pub const MEDIA_HOST: &str = "play.itunes.apple.com";
/// Path prefix of local media URLs; followed by the media id.
pub const MEDIA_URL_PREFIX: &str = "/__cider-offline/media/";
/// Prefix of custom-song ids (`local-<custom song id>`).
pub const CUSTOM_SONG_PREFIX: &str = "local-";

const STREAM_CHUNK: usize = 64 * 1024;

/// Returns the absolute local media URL for a track or custom-song id.
pub fn media_url(media_id: &str) -> String {
    format!("https://{MEDIA_HOST}{MEDIA_URL_PREFIX}{media_id}")
}

/// Extracts the media id from a local media URL path.
pub(crate) fn media_id_from_path(path: &str) -> Option<&str> {
    path.strip_prefix(MEDIA_URL_PREFIX)
        .map(|rest| rest.split('/').next().unwrap_or_default())
        .filter(|id| is_valid_media_id(id))
}

/// Media ids are Apple ids (`1234`, `i.AbC`, `l.x-y`) or `local-<n>`.
pub(crate) fn is_valid_media_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Parses the custom-song database id from a `local-<n>` media id.
pub(crate) fn custom_song_id(media_id: &str) -> Option<i64> {
    media_id.strip_prefix(CUSTOM_SONG_PREFIX)?.parse().ok()
}

/// `Content-Type` for an audio file, by extension.
pub fn audio_content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "m4a" | "mp4" | "m4b" | "alac" => "audio/mp4",
        "aac" => "audio/aac",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        _ => "application/octet-stream",
    }
}

/// Extensions accepted as audio files.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "m4a", "mp4", "m4b", "alac", "aac", "mp3", "flac", "ogg", "oga", "opus", "wav", "webm",
];

/// Links audio files already in the media directory (`<catalog id>.<ext>`,
/// as every downloader writes them) to tracks that have no recorded audio,
/// so earlier downloads show as downloaded and play offline. Runs when the
/// download queue starts and after every library sync (a sync into a reset
/// store creates the track rows only then). Returns how many rows were linked.
pub(crate) async fn link_existing_media(storage: &Storage, media_dir: &Path) -> usize {
    let ids = match storage.ids_missing_audio().await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "could not list tracks missing audio");
            return 0;
        }
    };
    let mut linked = 0usize;
    for id in ids {
        if !is_valid_media_id(&id) {
            continue;
        }
        let Some(path) = existing_media_file(media_dir, &id).await else {
            continue;
        };
        match storage.set_audio_by_any_id(&id, Some(path)).await {
            Ok(changed) => linked += changed,
            Err(error) => tracing::warn!(%id, %error, "could not link media file"),
        }
    }
    if linked > 0 {
        tracing::info!(linked, "linked existing media files to tracks");
    }
    linked
}

/// The first existing `<media_dir>/<id>.<ext>` audio file.
async fn existing_media_file(media_dir: &Path, id: &str) -> Option<PathBuf> {
    for ext in AUDIO_EXTENSIONS {
        let path = media_dir.join(format!("{id}.{ext}"));
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|meta| meta.is_file())
        {
            return Some(path);
        }
    }
    None
}

/// A track's playable local file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAudio {
    /// Id used in the media URL.
    pub media_id: String,
    /// The file.
    pub path: PathBuf,
    /// Duration, if known.
    pub duration_ms: Option<u32>,
    /// Title, if known.
    pub title: Option<String>,
    /// Artist, if known.
    pub artist: Option<String>,
    /// Album, if known.
    pub album: Option<String>,
}

/// Resolves ids to local audio files: custom songs (`local-<n>`, by audio mode)
/// first, then real tracks via track mapping (by id or catalog id) and the
/// track's `audio_path`. Only files that exist are returned.
#[derive(Debug, Clone)]
pub(crate) struct MediaResolver {
    storage: Storage,
}

impl MediaResolver {
    pub(crate) fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Resolves a playback / media id to a local file. A custom song
    /// (`local-<n>`) resolves by its audio mode: a copied local file, or — in
    /// reference mode — the file of the real catalog track it points at. The
    /// returned [`LocalAudio::media_id`] stays the requested id, so the media
    /// URL keeps pointing at the custom song.
    pub(crate) async fn resolve(&self, id: &str) -> Result<Option<LocalAudio>, StorageError> {
        if custom_song_id(id).is_some() {
            return self.resolve_custom(id).await;
        }
        self.resolve_real(id).await
    }

    /// Resolves a custom song id (`local-<n>`).
    async fn resolve_custom(&self, id: &str) -> Result<Option<LocalAudio>, StorageError> {
        let Some(track) = self.storage.get_track(id).await? else {
            return Ok(None);
        };
        // A row carrying a `local-<n>` id but somehow not marked custom still
        // resolves through the real path rather than being dropped.
        if !track.is_custom {
            return self.resolve_real(id).await;
        }
        let path = match track.audio_mode {
            // Reference mode dereferences the real catalog id and serves that
            // track's file; the DRM downloader only ever sees that catalog id.
            Some(CustomAudioMode::Reference) => match &track.reference_track_id {
                Some(reference) => {
                    let record = self.storage.find_track(reference).await?;
                    self.find_file(reference, record.as_ref()).await?
                }
                None => None,
            },
            // Local copy, or no audio chosen yet (no `audio_path`).
            Some(CustomAudioMode::LocalCopy) | None => match track.audio_path.clone() {
                Some(path) if exists(&path).await => Some(path),
                _ => None,
            },
        };
        Ok(path.map(|path| LocalAudio {
            media_id: id.to_owned(),
            path,
            duration_ms: track.duration_ms,
            title: Some(track.title.clone()),
            artist: Some(track.artist.clone()),
            album: track.album.clone(),
        }))
    }

    /// Resolves a real (non-custom) track id to its local file plus metadata.
    async fn resolve_real(&self, id: &str) -> Result<Option<LocalAudio>, StorageError> {
        let record = self.storage.find_track(id).await?;
        let path = self.find_file(id, record.as_ref()).await?;
        Ok(path.map(|path| LocalAudio {
            media_id: id.to_owned(),
            path,
            duration_ms: record.as_ref().and_then(|r| r.track.duration_ms),
            title: record.as_ref().map(|r| r.track.title.clone()),
            artist: record.as_ref().map(|r| r.track.artist.clone()),
            album: record.as_ref().and_then(|r| r.track.album.clone()),
        }))
    }

    /// Finds the existing local file for a real track: a track mapping (by id,
    /// row id or catalog id), then the track row's `audio_path`.
    async fn find_file(
        &self,
        id: &str,
        record: Option<&TrackRecord>,
    ) -> Result<Option<PathBuf>, StorageError> {
        let mut candidate_ids = vec![id.to_owned()];
        if let Some(record) = record {
            candidate_ids.push(record.track.id.clone());
            candidate_ids.extend(record.catalog_id.clone());
        }
        candidate_ids.dedup();
        for candidate in &candidate_ids {
            if let Some(mapping) = self.storage.get_track_mapping(candidate).await?
                && exists(&mapping.file_path).await
            {
                return Ok(Some(mapping.file_path));
            }
        }
        if let Some(audio) = record.and_then(|r| r.track.audio_path.clone())
            && exists(&audio).await
        {
            return Ok(Some(audio));
        }
        Ok(None)
    }
}

async fn exists(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

/// A parsed single byte range, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

/// Outcome of evaluating a `Range` header against a file size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeRequest {
    /// No (usable) range: send the whole file.
    Full,
    /// A satisfiable single range.
    Partial(ByteRange),
    /// The range cannot be satisfied (`416`).
    Unsatisfiable,
}

/// Evaluates a `Range` header (RFC 9110 §14). Multiple ranges and unknown
/// units are ignored (whole file), which the RFC permits.
pub(crate) fn parse_range(header: Option<&str>, size: u64) -> RangeRequest {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return RangeRequest::Full;
    };
    let (start, end) = (start.trim(), end.trim());
    let range = if start.is_empty() {
        // Suffix range: last N bytes.
        let Ok(suffix) = end.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if suffix == 0 || size == 0 {
            return RangeRequest::Unsatisfiable;
        }
        ByteRange {
            start: size.saturating_sub(suffix),
            end: size - 1,
        }
    } else {
        let Ok(start) = start.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if start >= size {
            return RangeRequest::Unsatisfiable;
        }
        let end = if end.is_empty() {
            size - 1
        } else {
            match end.parse::<u64>() {
                Ok(end) if end >= start => end.min(size - 1),
                _ => return RangeRequest::Full,
            }
        };
        ByteRange { start, end }
    };
    RangeRequest::Partial(range)
}

/// Serves a file with `Range` support, streaming from disk.
///
/// `HEAD` returns headers only. Errors opening the file yield `404`.
pub(crate) async fn serve_file(
    path: &Path,
    method: &Method,
    request_headers: &HeaderMap,
) -> Response<Body> {
    match serve_file_inner(path, method, request_headers).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to serve local media");
            status_only(StatusCode::NOT_FOUND, request_headers)
        }
    }
}

fn status_only(status: StatusCode, request_headers: &HeaderMap) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    add_cors(response.headers_mut(), request_headers);
    response
        .headers_mut()
        .insert(LOCAL_HEADER, HeaderValue::from_static("media"));
    response
}

async fn serve_file_inner(
    path: &Path,
    method: &Method,
    request_headers: &HeaderMap,
) -> std::io::Result<Response<Body>> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let range_header = request_headers.get(RANGE).and_then(|v| v.to_str().ok());

    let (status, start, length) = match parse_range(range_header, size) {
        RangeRequest::Full => (StatusCode::OK, 0, size),
        RangeRequest::Partial(range) => (
            StatusCode::PARTIAL_CONTENT,
            range.start,
            range.end - range.start + 1,
        ),
        RangeRequest::Unsatisfiable => {
            let mut response = status_only(StatusCode::RANGE_NOT_SATISFIABLE, request_headers);
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(CONTENT_RANGE, value);
            }
            return Ok(response);
        }
    };

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        if start > 0 {
            file.seek(SeekFrom::Start(start)).await?;
        }
        Body::from_stream(ReaderStream::with_capacity(file.take(length), STREAM_CHUNK))
    };

    let mut response = status_only(status, request_headers);
    *response.body_mut() = body;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(audio_content_type(path)),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(value) =
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + length - 1))
    {
        headers.insert(CONTENT_RANGE, value);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::body::read_body;
    use crate::storage::TrackMapping;

    #[test]
    fn open_ended_range_should_run_to_end() {
        assert_eq!(
            parse_range(Some("bytes=10-"), 100),
            RangeRequest::Partial(ByteRange { start: 10, end: 99 })
        );
    }

    #[test]
    fn suffix_range_should_take_last_bytes() {
        assert_eq!(
            parse_range(Some("bytes=-10"), 100),
            RangeRequest::Partial(ByteRange { start: 90, end: 99 })
        );
    }

    #[test]
    fn end_past_size_should_be_clamped() {
        assert_eq!(
            parse_range(Some("bytes=0-1000"), 100),
            RangeRequest::Partial(ByteRange { start: 0, end: 99 })
        );
    }

    #[test]
    fn start_past_size_should_be_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=100-"), 100),
            RangeRequest::Unsatisfiable
        );
    }

    #[test]
    fn multiple_ranges_should_fall_back_to_full() {
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), RangeRequest::Full);
    }

    #[test]
    fn media_id_should_reject_traversal() {
        assert_eq!(media_id_from_path("/__cider-offline/media/..%2F"), None);
    }

    #[test]
    fn media_id_should_parse_library_id() {
        assert_eq!(
            media_id_from_path("/__cider-offline/media/i.AbC-1"),
            Some("i.AbC-1")
        );
    }

    #[test]
    fn content_type_should_follow_extension() {
        assert_eq!(audio_content_type(Path::new("a.FLAC")), "audio/flac");
    }

    fn sample_file(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("song.mp3");
        std::fs::write(&path, b"0123456789").unwrap();
        path
    }

    fn range_headers(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static(value));
        headers
    }

    #[tokio::test]
    async fn range_request_should_return_partial_content() {
        let dir = tempfile::tempdir().unwrap();
        let response = serve_file(
            &sample_file(&dir),
            &Method::GET,
            &range_headers("bytes=2-4"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes 2-4/10");
        assert_eq!(&read_body(response.into_body()).await.unwrap()[..], b"234");
    }

    #[tokio::test]
    async fn full_request_should_return_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let response = serve_file(&sample_file(&dir), &Method::GET, &HeaderMap::new()).await;
        assert_eq!(response.headers()[CONTENT_TYPE], "audio/mpeg");
        assert_eq!(read_body(response.into_body()).await.unwrap().len(), 10);
    }

    #[tokio::test]
    async fn unsatisfiable_range_should_return_416() {
        let dir = tempfile::tempdir().unwrap();
        let response = serve_file(
            &sample_file(&dir),
            &Method::GET,
            &range_headers("bytes=50-"),
        )
        .await;
        assert_eq!(response.headers()[CONTENT_RANGE], "bytes */10");
    }

    #[tokio::test]
    async fn missing_file_should_return_404() {
        let response = serve_file(Path::new("/nope/x.mp3"), &Method::GET, &HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resolver_should_prefer_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let file = sample_file(&dir);
        storage
            .set_track_mapping(TrackMapping {
                track_id: "77".to_owned(),
                file_path: file.clone(),
            })
            .await
            .unwrap();
        let audio = MediaResolver::new(storage)
            .resolve("77")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audio.path, file);
    }

    #[tokio::test]
    async fn resolver_should_ignore_missing_files() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .set_track_mapping(TrackMapping {
                track_id: "77".to_owned(),
                file_path: PathBuf::from("/definitely/missing.mp3"),
            })
            .await
            .unwrap();
        assert!(
            MediaResolver::new(storage)
                .resolve("77")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn resolver_should_map_library_id_to_catalog_keyed_file() {
        use crate::storage::{CatalogUpdate, TrackMetadata};

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let file = sample_file(&dir);
        // A library track whose catalog id is 555.
        storage
            .apply_catalog_update(CatalogUpdate {
                tracks: vec![TrackMetadata {
                    id: "i.lib".to_owned(),
                    catalog_id: Some("555".to_owned()),
                    title: "T".to_owned(),
                    artist: "A".to_owned(),
                    album: None,
                    duration_ms: Some(1000),
                    artwork_url: None,
                    metadata: None,
                    ..TrackMetadata::default()
                }],
                ..CatalogUpdate::default()
            })
            .await
            .unwrap();
        // The file was downloaded / mapped under the catalog id.
        storage
            .set_track_mapping(TrackMapping {
                track_id: "555".to_owned(),
                file_path: file.clone(),
            })
            .await
            .unwrap();
        // A replay carrying the library id still resolves to that file.
        let audio = MediaResolver::new(storage)
            .resolve("i.lib")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audio.path, file);
    }

    #[tokio::test]
    async fn resolver_should_find_custom_song() {
        use crate::storage::{CustomAudio, NewCustomTrack};

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "T".to_owned(),
                artist: "A".to_owned(),
                audio: CustomAudio::LocalCopy(sample_file(&dir)),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let audio = MediaResolver::new(storage).resolve(&id).await.unwrap();
        assert!(audio.is_some());
    }

    #[tokio::test]
    async fn resolver_should_dereference_reference_audio_to_catalog_file() {
        use crate::storage::{CustomAudio, NewCustomTrack};

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let file = sample_file(&dir);
        // The real catalog track 555 has a mapped local file (the DRM side only
        // ever keys on this catalog id).
        storage
            .set_track_mapping(TrackMapping {
                track_id: "555".to_owned(),
                file_path: file.clone(),
            })
            .await
            .unwrap();
        // A custom song whose audio references that catalog id.
        let id = storage
            .create_custom_track(NewCustomTrack {
                title: "Ref".to_owned(),
                artist: "A".to_owned(),
                audio: CustomAudio::Reference("555".to_owned()),
                ..NewCustomTrack::default()
            })
            .await
            .unwrap();
        let audio = MediaResolver::new(storage)
            .resolve(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audio.path, file);
        // The media URL keeps pointing at the custom song, not the catalog id.
        assert_eq!(audio.media_id, id);
    }
}
