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
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use hudsucker::Body;
use hudsucker::hyper::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, RANGE,
};
use hudsucker::hyper::{HeaderMap, Method, Response, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, ReadBuf};
use tokio_util::io::ReaderStream;

use crate::respond::{LOCAL_HEADER, add_cors};
use crate::storage::{AudioVariant, CustomAudioMode, Storage, StorageError, TrackRecord};

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

/// Links audio files already in the media directory (`<catalog id>.<ext>`
/// and `<catalog id>.spatial.m4a`, as every downloader writes them) to tracks that have no recorded audio,
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
        let spatial = media_dir.join(crate::download::media_file_name(&id, AudioVariant::Spatial));
        let files = [
            (
                AudioVariant::Stereo,
                existing_media_file(media_dir, &id).await,
            ),
            (
                AudioVariant::Spatial,
                is_file(&spatial).await.then_some(spatial),
            ),
        ];
        for (variant, path) in files {
            let Some(path) = path else {
                continue;
            };
            match storage.set_audio_by_any_id(&id, variant, Some(path)).await {
                Ok(changed) => {
                    linked += changed;
                    if changed > 0 {
                        pin_linked(storage, &id).await;
                    }
                }
                Err(error) => tracing::warn!(%id, %error, "could not link media file"),
            }
        }
    }
    if linked > 0 {
        tracing::info!(linked, "linked existing media files to tracks");
    }
    linked
}

/// Gives a song whose file was just linked an individual pin when nothing
/// pins it yet, so it doesn't count as an unlinked download (after a
/// database reset every downloaded file is linked this way).
async fn pin_linked(storage: &Storage, id: &str) {
    match storage.download_owners(id).await {
        Ok(owners) if owners.is_empty() => {
            if let Err(error) = storage.pin_downloads(None, vec![id.to_owned()]).await {
                tracing::warn!(%id, %error, "could not pin a linked media file");
            }
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%id, %error, "could not read the pins of a linked file"),
    }
}

/// The first existing `<media_dir>/<id>.<ext>` audio file.
async fn existing_media_file(media_dir: &Path, id: &str) -> Option<PathBuf> {
    for ext in AUDIO_EXTENSIONS {
        let path = media_dir.join(format!("{id}.{ext}"));
        if is_file(&path).await {
            return Some(path);
        }
    }
    None
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

/// A track's playable local file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAudio {
    /// Id used in the media URL.
    pub media_id: String,
    /// Which version of the song the file holds (a mapped or custom file
    /// counts as [`AudioVariant::Stereo`]).
    pub variant: AudioVariant,
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

impl LocalAudio {
    /// The media URL serving exactly this file (see [`variant_media_url`]).
    pub fn url(&self) -> String {
        variant_media_url(&self.media_id, self.variant)
    }
}

/// The media URL of one variant of a track: the plain [`media_url`] for
/// [`AudioVariant::Stereo`] (unchanged from before variants existed), and
/// `<media url>/spatial` for [`AudioVariant::Spatial`].
pub fn variant_media_url(media_id: &str, variant: AudioVariant) -> String {
    match variant {
        AudioVariant::Stereo => media_url(media_id),
        AudioVariant::Spatial => format!("{}/{variant}", media_url(media_id)),
    }
}

/// The variant a local media URL path names (`<prefix><id>/normal` or
/// `<prefix><id>/spatial`); `None` for the plain `<prefix><id>`, which serves
/// the preferred file.
pub(crate) fn media_variant_from_path(path: &str) -> Option<AudioVariant> {
    let rest = path.strip_prefix(MEDIA_URL_PREFIX)?;
    let mut segments = rest.split('/');
    segments.next()?;
    segments.next().and_then(AudioVariant::parse)
}

/// The local files of one track, per variant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalFiles {
    /// The normal file (a download, a mapping or a custom song's file).
    pub(crate) normal: Option<LocalAudio>,
    /// The spatial download.
    pub(crate) spatial: Option<LocalAudio>,
}

impl LocalFiles {
    /// The file of `variant`, if present.
    pub(crate) fn get(&self, variant: AudioVariant) -> Option<&LocalAudio> {
        match variant {
            AudioVariant::Stereo => self.normal.as_ref(),
            AudioVariant::Spatial => self.spatial.as_ref(),
        }
    }

    /// Takes the file of `variant`, if present.
    pub(crate) fn take(&mut self, variant: AudioVariant) -> Option<LocalAudio> {
        match variant {
            AudioVariant::Stereo => self.normal.take(),
            AudioVariant::Spatial => self.spatial.take(),
        }
    }
}

/// Resolves ids to local audio files: custom songs (`local-<n>`, by audio mode)
/// first, then real tracks via track mapping (by id or catalog id) and the
/// track's recorded audio, per variant. Only files that exist are returned.
#[derive(Debug, Clone)]
pub(crate) struct MediaResolver {
    storage: Storage,
}

impl MediaResolver {
    pub(crate) fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Resolves a playback / media id to its preferred local file: normal,
    /// else spatial. See [`Self::files`].
    pub(crate) async fn resolve(&self, id: &str) -> Result<Option<LocalAudio>, StorageError> {
        let mut files = self.files(id).await?;
        Ok(files
            .take(AudioVariant::Stereo)
            .or_else(|| files.take(AudioVariant::Spatial)))
    }

    /// Resolves a playback / media id to its local file of `variant`, if
    /// that file exists.
    pub(crate) async fn resolve_variant(
        &self,
        id: &str,
        variant: AudioVariant,
    ) -> Result<Option<LocalAudio>, StorageError> {
        Ok(self.files(id).await?.take(variant))
    }

    /// Every existing local file of a playback / media id.
    ///
    /// A custom song (`local-<n>`) resolves by its audio mode: a copied local
    /// file (normal), or (in reference mode) the files of the real catalog
    /// track it points at. The returned [`LocalAudio::media_id`] stays the
    /// requested id, so the media URL keeps pointing at the custom song.
    pub(crate) async fn files(&self, id: &str) -> Result<LocalFiles, StorageError> {
        if custom_song_id(id).is_some() {
            return self.files_custom(id).await;
        }
        self.files_real(id).await
    }

    /// Files of a custom song id (`local-<n>`).
    async fn files_custom(&self, id: &str) -> Result<LocalFiles, StorageError> {
        let Some(track) = self.storage.get_track(id).await? else {
            return Ok(LocalFiles::default());
        };
        // A row carrying a `local-<n>` id but somehow not marked custom still
        // resolves through the real path rather than being dropped.
        if !track.is_custom {
            return self.files_real(id).await;
        }
        let [normal, spatial] = match track.audio_mode {
            // Reference mode dereferences the real catalog id and serves that
            // track's files; the DRM downloader only ever sees that catalog id.
            Some(CustomAudioMode::Reference) => match &track.reference_track_id {
                Some(reference) => {
                    let record = self.storage.find_track(reference).await?;
                    self.find_files(reference, record.as_ref()).await?
                }
                None => [None, None],
            },
            // Local copy, or no audio chosen yet (no audio recorded).
            Some(CustomAudioMode::LocalCopy) | None => match track.audio.preferred() {
                Some(path) if exists(path).await => [Some(path.to_path_buf()), None],
                _ => [None, None],
            },
        };
        let audio = |variant: AudioVariant, path: PathBuf| LocalAudio {
            media_id: id.to_owned(),
            variant,
            path,
            duration_ms: track.duration_ms,
            title: Some(track.title.clone()),
            artist: Some(track.artist.clone()),
            album: track.album.clone(),
        };
        Ok(LocalFiles {
            normal: normal.map(|path| audio(AudioVariant::Stereo, path)),
            spatial: spatial.map(|path| audio(AudioVariant::Spatial, path)),
        })
    }

    /// Files of a real (non-custom) track id, plus metadata.
    async fn files_real(&self, id: &str) -> Result<LocalFiles, StorageError> {
        let record = self.storage.find_track(id).await?;
        let [normal, spatial] = self.find_files(id, record.as_ref()).await?;
        let audio = |variant: AudioVariant, path: PathBuf| LocalAudio {
            media_id: id.to_owned(),
            variant,
            path,
            duration_ms: record.as_ref().and_then(|r| r.track.duration_ms),
            title: record.as_ref().map(|r| r.track.title.clone()),
            artist: record.as_ref().map(|r| r.track.artist.clone()),
            album: record.as_ref().and_then(|r| r.track.album.clone()),
        };
        Ok(LocalFiles {
            normal: normal.map(|path| audio(AudioVariant::Stereo, path)),
            spatial: spatial.map(|path| audio(AudioVariant::Spatial, path)),
        })
    }

    /// Finds the existing local files of a real track, `[normal, spatial]`.
    /// Normal is a track mapping (by id, row id or catalog id), else the
    /// track row's recorded normal file; spatial is the recorded spatial file.
    async fn find_files(
        &self,
        id: &str,
        record: Option<&TrackRecord>,
    ) -> Result<[Option<PathBuf>; 2], StorageError> {
        let mut candidate_ids = vec![id.to_owned()];
        if let Some(record) = record {
            candidate_ids.push(record.track.id.clone());
            candidate_ids.extend(record.catalog_id.clone());
        }
        candidate_ids.dedup();
        let mut normal = None;
        for candidate in &candidate_ids {
            if let Some(mapping) = self.storage.get_track_mapping(candidate).await?
                && exists(&mapping.file_path).await
            {
                normal = Some(mapping.file_path);
                break;
            }
        }
        if normal.is_none() {
            normal = existing(record.and_then(|r| r.track.audio.get(AudioVariant::Stereo))).await;
        }
        let spatial = existing(record.and_then(|r| r.track.audio.get(AudioVariant::Spatial))).await;
        Ok([normal, spatial])
    }
}

/// `path` as an owned path when it names an existing file.
async fn existing(path: Option<&Path>) -> Option<PathBuf> {
    match path {
        Some(path) if exists(path).await => Some(path.to_path_buf()),
        _ => None,
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

/// Wraps a media body reader and logs, at debug level, how much of it was
/// read when the body is dropped. Fewer bytes than expected means the client
/// closed the request early (players cancel range requests routinely).
///
/// The count is what the body stream pulled from disk, which can run up to
/// one chunk ahead of what actually reached the socket.
struct LoggedReader<R> {
    inner: R,
    file: String,
    start: u64,
    expected: u64,
    sent: u64,
    opened: Instant,
}

impl<R> LoggedReader<R> {
    fn new(inner: R, path: &Path, start: u64, expected: u64) -> Self {
        Self {
            inner,
            file: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            start,
            expected,
            sent: 0,
            opened: Instant::now(),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for LoggedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        let read = buf.filled().len().saturating_sub(before);
        self.sent = self.sent.saturating_add(read as u64);
        result
    }
}

impl<R> Drop for LoggedReader<R> {
    fn drop(&mut self) {
        tracing::debug!(
            file = %self.file,
            start = self.start,
            expected = self.expected,
            sent = self.sent,
            aborted = self.sent < self.expected,
            elapsed_ms = self.opened.elapsed().as_millis(),
            "local media body finished"
        );
    }
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
        let reader = LoggedReader::new(file.take(length), path, start, length);
        Body::from_stream(ReaderStream::with_capacity(reader, STREAM_CHUNK))
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
    fn media_variant_should_be_read_from_the_path() {
        assert_eq!(
            (
                media_variant_from_path("/__cider-offline/media/1"),
                media_variant_from_path("/__cider-offline/media/1/spatial"),
                media_variant_from_path("/__cider-offline/media/1/stereo"),
                media_id_from_path("/__cider-offline/media/1/spatial"),
            ),
            (
                None,
                Some(AudioVariant::Spatial),
                Some(AudioVariant::Stereo),
                Some("1")
            )
        );
    }

    #[test]
    fn variant_media_url_should_keep_the_plain_url_for_normal() {
        assert_eq!(
            (
                variant_media_url("a.1", AudioVariant::Stereo),
                variant_media_url("a.1", AudioVariant::Spatial),
            ),
            (
                "https://play.itunes.apple.com/__cider-offline/media/a.1".to_owned(),
                "https://play.itunes.apple.com/__cider-offline/media/a.1/spatial".to_owned()
            )
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
