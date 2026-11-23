//! Widevine DRM downloader for Cider Offline.
//!
//! Implements the [`Downloader`] trait from `cider-offline-core`. Given a track
//! id it fetches the Apple Music playback manifest, negotiates a Widevine
//! license, downloads the encrypted audio, decrypts it, and writes the
//! resulting `.m4a` into the media directory.
//!
//! # Wiring it up
//!
//! ```no_run
//! use cider_offline_core::{Config, ProxyHandle, Storage};
//! use cider_offline_download::DrmDownloader;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let storage = Storage::open_async("store.sqlite".into()).await?;
//! let drm = DrmDownloader::with_default_device(storage)?;
//!
//! let handle = ProxyHandle::builder(Config::default())
//!     .downloader(drm)
//!     .start()
//!     .await?;
//! # Ok(()) }
//! ```

mod apple;
mod cdm;
mod cenc;
mod duration;
mod error;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::prelude::*;
use cider_offline_core::CancellationToken;
use cider_offline_core::download::{
    DownloadError, DownloadProgress, DownloadRequest, DownloadStage, Downloaded, Downloader,
    DownloaderCapabilities,
};
use cider_offline_core::storage::{AudioVariant, Storage, TokenKind};
use reqwest::header::HeaderMap;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::apple::{Apple, Manifest, SegmentRange};
use crate::cenc::ContentKeys;

pub use cdm::build_pssh_box;
pub use error::DrmError;

/// Registry id for this downloader.
pub const DRM_DOWNLOADER_ID: &str = "drm";

/// Max concurrent segment downloads from the aod-ssl CDN. A small bound keeps
/// the request pattern close to what a real browser issues.
const SEGMENT_CONCURRENCY: usize = 6;

/// Pause between the license requests of one spatial download (one per key
/// group). Apple refuses licenses when many arrive within a second; two a
/// fraction of a second apart match what Cider itself sends.
const LICENSE_SPACING: Duration = Duration::from_millis(400);

/// Bundled fallback WVD device blob. Used when no user-provided
/// `device.wvd` is found in the app data directory. Enables DRM
/// downloads out of the box.
pub const DEFAULT_WVD_BYTES: &[u8] = include_bytes!("default_device.wvd");

/// Downloads audio by acquiring a Widevine license from Apple Music,
/// downloading the encrypted stream, and decrypting it locally.
pub struct DrmDownloader {
    storage: Storage,
    client: reqwest::Client,
    cdm: cdm::Cdm,
}

/// Apple Music auth tokens required for every DRM request.
///
/// A named-field struct (instead of `(String, String)`) so a swap of the two
/// values is a compile error, and a hand-written `Debug` keeps the token
/// bodies out of logs and panic messages.
#[derive(Clone)]
pub struct Tokens {
    /// Apple Music developer token (the `Authorization: Bearer …` value).
    pub developer: String,
    /// Apple Music user token (the `x-apple-music-user-token` header).
    pub media_user: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("developer", &"<redacted>")
            .field("media_user", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for DrmDownloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrmDownloader")
            .field("id", &DRM_DOWNLOADER_ID)
            .finish_non_exhaustive()
    }
}

impl DrmDownloader {
    /// Creates a new DRM downloader.
    ///
    /// `wvd_base64` is the base64-encoded Widevine Device blob.
    /// `storage` is used to read the captured Apple Music auth tokens.
    ///
    /// # Errors
    ///
    /// Returns an error if the WVD data is invalid, or the HTTP client
    /// cannot be built (TLS/transport configuration failure).
    pub fn new(storage: Storage, wvd_base64: &str) -> Result<Self, DrmError> {
        Ok(Self {
            storage,
            client: default_client()?,
            cdm: cdm::Cdm::from_wvd_base64(wvd_base64)?,
        })
    }

    /// Creates a new DRM downloader from raw WVD bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the WVD data is invalid, or the HTTP client
    /// cannot be built (TLS/transport configuration failure).
    pub fn from_wvd_bytes(storage: Storage, wvd_bytes: &[u8]) -> Result<Self, DrmError> {
        Ok(Self {
            storage,
            client: default_client()?,
            cdm: cdm::Cdm::from_wvd(wvd_bytes)?,
        })
    }

    /// Creates a new DRM downloader using the bundled fallback WVD device.
    ///
    /// Use this when the user has not supplied their own `device.wvd`.
    ///
    /// # Errors
    ///
    /// Returns an error if the bundled WVD data fails to parse, or the HTTP
    /// client cannot be built.
    pub fn with_default_device(storage: Storage) -> Result<Self, DrmError> {
        Self::from_wvd_bytes(storage, DEFAULT_WVD_BYTES)
    }

    /// Creates a new DRM downloader with a custom `reqwest::Client`.
    ///
    /// # Errors
    ///
    /// Returns an error if the WVD data is invalid.
    pub fn with_client(
        storage: Storage,
        wvd_base64: &str,
        client: reqwest::Client,
    ) -> Result<Self, DrmError> {
        Ok(Self {
            storage,
            client,
            cdm: cdm::Cdm::from_wvd_base64(wvd_base64)?,
        })
    }

    async fn tokens(&self) -> Result<Tokens, DrmError> {
        let developer = self
            .storage
            .get_auth_token(TokenKind::DeveloperToken)
            .await?
            .ok_or(DrmError::MissingTokens)?;
        let media_user = self
            .storage
            .get_auth_token(TokenKind::MediaUserToken)
            .await?
            .ok_or(DrmError::MissingTokens)?;
        Ok(Tokens {
            developer,
            media_user,
        })
    }
}

/// Builds the default HTTP client used by `DrmDownloader`.
///
/// Sets `connect_timeout(10s)` so a stalled DNS/TCP handshake never hangs
/// forever, and `read_timeout(30s)` so a dead CDN connection between reads
/// also unblocks. A total `timeout` is deliberately omitted because song +
/// video bodies routinely take tens of seconds to stream.
fn default_client() -> Result<reqwest::Client, DrmError> {
    reqwest::Client::builder()
        .user_agent(apple::USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(DrmError::Http)
}

#[async_trait]
impl Downloader for DrmDownloader {
    fn id(&self) -> &'static str {
        DRM_DOWNLOADER_ID
    }

    fn capabilities(&self) -> DownloaderCapabilities {
        DownloaderCapabilities::new()
            .with_music_videos(true)
            .with_spatial(true)
    }

    async fn fetch(&self, request: &DownloadRequest) -> Result<Downloaded, DownloadError> {
        fetch_inner(self, request)
            .await
            .map_err(DownloadError::from)
    }
}

/// Reports `stage` when the request has a progress sink.
fn report_stage(progress: Option<&DownloadProgress>, stage: DownloadStage) {
    if let Some(progress) = progress {
        progress.stage(stage);
    }
}

/// The encrypted file and the keys that decrypt it.
struct Encrypted {
    bytes: Vec<u8>,
    keys: ContentKeys,
}

async fn fetch_inner(
    dl: &DrmDownloader,
    request: &DownloadRequest,
) -> Result<Downloaded, DrmError> {
    let Tokens {
        developer: auth_token,
        media_user: media_user_token,
    } = dl.tokens().await?;
    let apple = Apple::new(dl.client.clone(), &auth_token, &media_user_token)?;
    let adam_id = request.playback.adam_id();
    let variant = request.variant;

    tracing::info!(adam_id = %adam_id, kind = ?request.kind, %variant, "fetching playback info");

    let encrypted = match variant {
        AudioVariant::Stereo => fetch_normal(dl, &apple, request).await?,
        AudioVariant::Spatial => fetch_spatial(dl, &apple, request).await?,
    };

    tracing::info!(
        adam_id = %adam_id,
        size = encrypted.bytes.len(),
        "decrypting"
    );

    // Decrypt CENC fMP4, then write the real duration into its headers.
    // Cancellation point right before the CPU-heavy step (the queue's
    // contract: check between segments and before decryption), and the work
    // runs on a blocking worker so a slow decrypt cannot stall the runtime.
    report_stage(request.progress.as_ref(), DownloadStage::Decrypting);
    if request.cancel.is_cancelled() {
        return Err(DrmError::Cancelled);
    }
    let (decrypted, duration_ms) = tokio::task::spawn_blocking(move || {
        let mut decrypted = cenc::decrypt(encrypted.bytes, &encrypted.keys)?;
        // A file whose duration cannot be written still plays (players
        // scan the fragments), so this never fails the download.
        let duration_ms = match duration::write_duration(&mut decrypted) {
            Ok(millis) => Some(millis),
            Err(error) => {
                tracing::warn!(%error, "could not write the file duration");
                None
            }
        };
        Ok::<_, DrmError>((decrypted, duration_ms))
    })
    .await
    .map_err(|e| DrmError::Join(e.to_string()))??;

    // Write atomically to request.output_path via `tempfile::NamedTempFile`:
    // create a uniquely-named temp file in the destination directory, write
    // all bytes, `sync_all` so the data hits disk before the rename, then
    // `persist` to swap it into place. The temp file is deleted on drop if
    // anything fails, so there is no `.m4a.part` to garbage-collect.
    //
    // `tempfile` is a synchronous std::fs API, so it runs on
    // `spawn_blocking` to avoid stalling tokio workers (same pattern as the
    // decrypt step above).
    report_stage(request.progress.as_ref(), DownloadStage::Saving);
    let dest = request.output_path.clone();
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let dest_for_write = dest.clone();
    let cancel = request.cancel.clone();
    tokio::task::spawn_blocking(move || -> Result<(), DrmError> {
        use std::io::Write as _;

        let dir = dest_for_write.parent().ok_or_else(|| {
            DrmError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "output_path has no parent directory",
            ))
        })?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(DrmError::Io)?;
        tmp.write_all(&decrypted).map_err(DrmError::Io)?;
        tmp.as_file().sync_all().map_err(DrmError::Io)?;
        // Last cancellation point: a cancelled download must not replace or
        // create the file. Returning drops `tmp`, which deletes it.
        if cancel.is_cancelled() {
            return Err(DrmError::Cancelled);
        }
        tmp.persist(&dest_for_write)
            .map_err(|e| DrmError::Io(e.error))?;
        Ok(())
    })
    .await
    .map_err(|e| DrmError::Join(e.to_string()))??;

    tracing::info!(
        adam_id = %adam_id,
        %variant,
        duration_ms = ?duration_ms,
        path = %dest.display(),
        "download complete"
    );

    Ok(Downloaded::new(dest, variant).with_duration_ms(duration_ms))
}

/// The normal version: webPlayback's `ctrp256` asset (or a music video's
/// audio rendition), one Widevine key for the whole file.
async fn fetch_normal(
    dl: &DrmDownloader,
    apple: &Apple,
    request: &DownloadRequest,
) -> Result<Encrypted, DrmError> {
    let playback = &request.playback;
    let adam_id = playback.adam_id();
    let progress = request.progress.as_ref();

    // 1. webPlayback: pick the song asset OR the music-video master playlist URL.
    report_stage(progress, DownloadStage::Requesting);
    let playback_info = apple.fetch_playback(playback, request.kind).await?;

    // 2. Resolve HLS manifest -> PSSH + segment list + init segment. For a song
    //    that is a single ranged file; for a music video it's the audio-only
    //    rendition's playlist (23 separate `.m4s` segments, no byte ranges).
    let hls = match &playback_info.manifest {
        Manifest::SongAsset { url, flavor } => {
            apple::fetch_hls_info(&dl.client, url, flavor).await?
        }
        Manifest::VideoMaster(url) => apple::fetch_video_hls_info(&dl.client, url).await?,
    };

    tracing::info!(
        adam_id = %adam_id,
        key_id = %cdm::hex_encode(&hls.pssh_init_data),
        segments = hls.segments.len(),
        "negotiating Widevine license"
    );

    // 3. widevineCert: binary service certificate.
    report_stage(progress, DownloadStage::License);
    let service_cert = apple.fetch_widevine_cert(&playback_info.cert_url).await?;

    // 4. Open CDM session with PSSH init-data + cert; produce challenge.
    //    `create_license_request` stores the raw LicenseRequest bytes on the
    //    session so `parse_license_response` can use them as the Widevine
    //    key-derivation context.
    let mut session = dl
        .cdm
        .open_session_with_cert(&hls.pssh_init_data, service_cert)?;
    let challenge = session.create_license_request()?;
    let challenge_b64 = BASE64_STANDARD.encode(&challenge);

    // 5. acquireWebPlaybackLicense: one request, one result. Core retries.
    let license_bytes = apple
        .acquire_license(
            &playback_info.license_url,
            &challenge_b64,
            &hls.key_uri,
            playback,
        )
        .await?;

    // 6. Parse license to recover the AES-128 content key.
    let content_key = cenc::content_key(&session.parse_license_response(&license_bytes)?)?;

    tracing::info!(
        adam_id = %adam_id,
        segments = hls.segments.len(),
        "downloading encrypted segments"
    );

    // 7. Download init + media segments (bounded concurrency), preserving order.
    //    Byte progress is reported from inside each per-URL chunk loop; cancel
    //    is checked between chunks so a Pause / remove during a long body read
    //    cleanly returns `DrmError::Cancelled`.
    report_stage(progress, DownloadStage::Fetching);
    let assembled = download_segments(
        &dl.client,
        &hls.init_segment,
        &hls.segments,
        progress,
        &request.cancel,
    )
    .await?;
    Ok(Encrypted {
        bytes: assembled.bytes,
        keys: ContentKeys::single(content_key),
    })
}

/// The spatial version: the binaural variant of the song's enhancedHls
/// master, with one Widevine key per key group (Apple uses two: `s1/e1` for
/// the first segment, the content key for the rest).
///
/// No webPlayback or widevineCert call is made: the playlists are public,
/// the license endpoint is fixed, and the challenge carries the client id in
/// cleartext as on the normal path. The license requests are spaced
/// [`LICENSE_SPACING`] apart.
async fn fetch_spatial(
    dl: &DrmDownloader,
    apple: &Apple,
    request: &DownloadRequest,
) -> Result<Encrypted, DrmError> {
    let playback = &request.playback;
    let adam_id = playback.adam_id();
    let progress = request.progress.as_ref();
    let unavailable = || DrmError::VariantUnavailable(AudioVariant::Spatial);

    report_stage(progress, DownloadStage::Requesting);
    let master = request.enhanced_hls.as_deref().ok_or_else(unavailable)?;
    let hls = apple::fetch_spatial_hls(&dl.client, master)
        .await?
        .ok_or_else(unavailable)?;

    tracing::info!(
        adam_id = %adam_id,
        key_groups = hls.key_groups.len(),
        segments = hls.segments.len(),
        "negotiating Widevine licenses for the spatial stream"
    );

    report_stage(progress, DownloadStage::License);
    let mut group_keys: Vec<[u8; 16]> = Vec::with_capacity(hls.key_groups.len());
    for (index, group) in hls.key_groups.iter().enumerate() {
        // The same key line twice needs one license only.
        if let Some(earlier) = hls.key_groups[..index]
            .iter()
            .position(|g| g.key_uri == group.key_uri)
        {
            group_keys.push(group_keys[earlier]);
            continue;
        }
        if index > 0 {
            tokio::select! {
                biased;
                () = request.cancel.cancelled() => return Err(DrmError::Cancelled),
                () = tokio::time::sleep(LICENSE_SPACING) => {}
            }
        }
        let mut session = dl.cdm.open_session(&group.pssh)?;
        let challenge = BASE64_STANDARD.encode(session.create_license_request()?);
        let license = apple
            .acquire_license(apple::LICENSE_URL, &challenge, &group.key_uri, playback)
            .await?;
        group_keys.push(cenc::content_key(
            &session.parse_license_response(&license)?,
        )?);
        tracing::debug!(
            adam_id = %adam_id,
            group = index,
            segments = ?group.segments,
            "spatial key group licensed"
        );
    }

    report_stage(progress, DownloadStage::Fetching);
    let assembled = download_segments(
        &dl.client,
        &hls.init_segment,
        &hls.segments,
        progress,
        &request.cancel,
    )
    .await?;
    let keys = spatial_keys(&assembled, &hls.key_groups, &group_keys)?;
    Ok(Encrypted {
        bytes: assembled.bytes,
        keys,
    })
}

/// Maps each sample description to its key group's key: every `moof` lies
/// in one segment, the segment in one key group, and the `moof` names its
/// sample description. A sample description reached from two groups with
/// different keys is an error.
fn spatial_keys(
    assembled: &Assembled,
    groups: &[apple::KeyGroup],
    group_keys: &[[u8; 16]],
) -> Result<ContentKeys, DrmError> {
    let mut by_index: Vec<(u32, [u8; 16])> = Vec::new();
    for (moof, index) in cenc::fragment_sample_descriptions(&assembled.bytes)? {
        // Piece 0 is the init segment; piece n + 1 is media segment n.
        let piece = assembled
            .starts
            .partition_point(|&start| start <= moof)
            .checked_sub(1)
            .ok_or_else(|| DrmError::Decrypt(format!("moof at {moof} precedes every segment")))?;
        let segment = piece
            .checked_sub(1)
            .ok_or_else(|| DrmError::Decrypt(format!("moof at {moof} lies in the init segment")))?;
        let group = groups
            .iter()
            .position(|g| g.segments.contains(&segment))
            .ok_or_else(|| DrmError::Decrypt(format!("segment {segment} has no key group")))?;
        let key = group_keys[group];
        match by_index.iter().find(|(i, _)| *i == index) {
            Some((_, known)) if *known != key => {
                return Err(DrmError::Decrypt(format!(
                    "sample description {index} is used by two key groups"
                )));
            }
            Some(_) => {}
            None => by_index.push((index, key)),
        }
    }
    tracing::debug!(
        sample_descriptions = ?by_index.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        "spatial keys mapped"
    );
    Ok(ContentKeys::by_sample_description(by_index))
}

/// Union of the ranges requested from a single URL.
///
/// When `fetch_full` is true at least one [`SegmentRange`] referenced the
/// whole resource (no `BYTERANGE`), so we must GET the full file. Otherwise
/// we send a single `Range: bytes=<min_off>-<max_end - 1>` request covering
/// every byte needed, then slice each segment out locally.
#[derive(Debug, Clone, Copy)]
struct UrlFetchPlan {
    fetch_full: bool,
    min_off: u64,
    max_end: u64,
}

impl UrlFetchPlan {
    fn empty() -> Self {
        Self {
            fetch_full: false,
            min_off: u64::MAX,
            max_end: 0,
        }
    }

    fn absorb(&mut self, byte_range: Option<(u64, u64)>) {
        match byte_range {
            None => self.fetch_full = true,
            Some((off, len)) => {
                self.min_off = self.min_off.min(off);
                self.max_end = self.max_end.max(off.saturating_add(len));
            }
        }
    }
}

/// Where the fetched bytes start inside the source resource. `buf[0]`
/// corresponds to byte `base_offset` of the URL.
#[derive(Debug, Clone)]
struct FetchedUrl {
    base_offset: u64,
    bytes: Vec<u8>,
}

/// Progress + cancellation handles threaded into each per-URL fetch task.
///
/// `done` is the atomic byte counter shared across all tasks fetching the
/// same download; `total` starts as the sum of the planned byterange widths
/// and grows as whole-file fetches reveal their `content_length()`.
#[derive(Clone)]
struct FetchContext {
    progress: Option<DownloadProgress>,
    cancel: CancellationToken,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

/// The downloaded file: the init segment and every media segment, in
/// playlist order.
struct Assembled {
    bytes: Vec<u8>,
    /// Offset in `bytes` where each piece starts: index 0 is the init
    /// segment, index `n + 1` media segment `n`.
    starts: Vec<usize>,
}

/// Downloads the init segment plus every media segment, honoring
/// `#EXT-X-BYTERANGE` / `#EXT-X-MAP:BYTERANGE`.
///
/// Each unique URL is fetched exactly once with a single `Range` request that
/// covers the union of byte ranges needed from it; disjoint segment slices
/// are then assembled locally in playlist order. This collapses Apple's
/// "20+1 byte-range segments on one media file" case into a single GET
/// while remaining correct for streams that genuinely span many URLs.
async fn download_segments(
    client: &reqwest::Client,
    init_segment: &SegmentRange,
    segments: &[SegmentRange],
    progress: Option<&DownloadProgress>,
    cancel: &CancellationToken,
) -> Result<Assembled, DrmError> {
    // 1. Assemble the ordered list of pieces: init first, then media segments.
    let mut refs: Vec<SegmentRange> = Vec::with_capacity(segments.len() + 1);
    refs.push(init_segment.clone());
    refs.extend(segments.iter().cloned());

    // 2. Compute each unique URL's union-range, preserving first-seen order
    //    so logs and traces stay deterministic.
    let mut plans: HashMap<String, UrlFetchPlan> = HashMap::new();
    let mut urls_ordered: Vec<String> = Vec::new();
    for r in &refs {
        let plan = plans.entry(r.url.clone()).or_insert_with(|| {
            urls_ordered.push(r.url.clone());
            UrlFetchPlan::empty()
        });
        plan.absorb(r.byte_range);
    }

    // 3. Known-byte total up front: the sum of planned ranges. `fetch_full`
    //    plans don't know their byte count yet; those contribute `0` here and
    //    each worker adds its `content_length()` to `total` the moment the
    //    server reveals it, so the progress ring stays monotonic and the
    //    reported fraction is correct for both songs (all ranged) and future
    //    video audio renditions (any of which may be whole-file fetches).
    let known_total: u64 = plans
        .values()
        .map(|p| {
            if p.fetch_full {
                0
            } else {
                p.max_end - p.min_off
            }
        })
        .sum();
    let ctx = FetchContext {
        progress: progress.cloned(),
        cancel: cancel.clone(),
        done: Arc::new(AtomicU64::new(0)),
        total: Arc::new(AtomicU64::new(known_total)),
    };
    if let Some(p) = &ctx.progress {
        p.report(0, Some(known_total));
    }

    // 4. Fetch each unique URL with bounded concurrency.
    let headers = apple::cdn_headers();
    let semaphore = Arc::new(Semaphore::new(SEGMENT_CONCURRENCY));
    let mut set: JoinSet<(String, Result<FetchedUrl, DrmError>)> = JoinSet::new();
    for url in &urls_ordered {
        let plan = plans[url];
        let url_owned = url.clone();
        let client = client.clone();
        let headers = headers.clone();
        let sem = semaphore.clone();
        let ctx = ctx.clone();
        set.spawn(async move {
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(e) => return (url_owned, Err(DrmError::Join(e.to_string()))),
            };
            let result = fetch_url_plan(&client, &url_owned, &headers, plan, &ctx).await;
            drop(permit);
            (url_owned, result)
        });
    }

    let mut fetched: HashMap<String, FetchedUrl> = HashMap::with_capacity(urls_ordered.len());
    while let Some(joined) = set.join_next().await {
        let (url, result) = joined.map_err(|e| DrmError::Join(e.to_string()))?;
        fetched.insert(url, result?);
    }

    // 5. Slice each piece out of the fetched buffers in playlist order.
    assemble_from_fetched(&refs, &fetched)
}

/// Fetches the subset of a URL described by `plan`. Returns the fetched bytes
/// together with the file offset they begin at (`base_offset`).
///
/// - A `Range`-less GET or a `200 OK` response for a `Range` request yields
///   the full file: `base_offset = 0`.
/// - A `206 Partial Content` response for a `Range` request yields only the
///   requested bytes: `base_offset = plan.min_off`.
async fn fetch_url_plan(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    plan: UrlFetchPlan,
    ctx: &FetchContext,
) -> Result<FetchedUrl, DrmError> {
    let mut req = client.get(url).headers(headers.clone());
    let want_range = !plan.fetch_full && plan.max_end > plan.min_off;
    if want_range {
        let header = format!("bytes={}-{}", plan.min_off, plan.max_end - 1);
        req = req.header(reqwest::header::RANGE, header);
    }
    let mut resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let len = usize::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0);
        return Err(DrmError::HttpStatus {
            endpoint: "segment",
            status: status.as_u16(),
            body_len: len,
            retry_after,
        });
    }
    // 206 => bytes start at the requested min_off; 200 => full file from 0.
    let is_partial = want_range && status == reqwest::StatusCode::PARTIAL_CONTENT;
    let base_offset = if is_partial { plan.min_off } else { 0 };

    // On a 206 confirm the server actually honoured our range: parse
    // `Content-Range: bytes <first>-<last>/<total>` and require
    // `first == plan.min_off` and `last >= plan.max_end - 1`. A mismatch
    // means the CDN gave us a different window and the later byte-slicing
    // would silently splice wrong bytes into the output. Also sanity-check
    // `Content-Length` when present: for a 206 it should equal the width
    // of the returned window. On any mismatch we fail with `RangeMismatch`
    // (a Transient error — CDN retries clear this).
    if is_partial {
        validate_partial_range(resp.headers(), plan)?;
    }

    // Whole-file fetches (no byterange in the manifest) contributed 0 to the
    // up-front total; fold in `content_length()` now that the server has
    // declared it, so the progress ring reflects the real workload.
    if plan.fetch_full
        && let Some(len) = resp.content_length()
    {
        ctx.total.fetch_add(len, Ordering::Relaxed);
    }

    // Stream the body into our buffer so we can report bytes as they arrive
    // and honour `cancel` between chunks. A buffered read also keeps peak
    // memory close to one body at a time instead of doubling through
    // `Bytes::to_vec()`.
    let capacity = usize::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    while let Some(chunk) = resp.chunk().await? {
        if ctx.cancel.is_cancelled() {
            return Err(DrmError::Cancelled);
        }
        bytes.extend_from_slice(&chunk);
        let now = ctx.done.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
        if let Some(p) = &ctx.progress {
            // `done` may briefly exceed `total` if a server overshoots the
            // range or advertised length; `max` keeps `report`'s invariant
            // `done <= total` without lying about progress downwards.
            let total_now = ctx.total.load(Ordering::Relaxed).max(now);
            p.report(now, Some(total_now));
        }
    }

    Ok(FetchedUrl { base_offset, bytes })
}

/// Validates that a 206 response's range headers cover the planned window.
///
/// Parses `Content-Range: bytes <first>-<last>/<total>` and requires
/// `first == plan.min_off` and `last + 1 >= plan.max_end`. Also checks
/// `Content-Length` against `last - first + 1` when both are present. Any
/// discrepancy surfaces as [`DrmError::RangeMismatch`] so the planner never
/// slices from a window the server didn't actually hand us.
fn validate_partial_range(
    headers: &reqwest::header::HeaderMap,
    plan: UrlFetchPlan,
) -> Result<(), DrmError> {
    let content_range = headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            DrmError::RangeMismatch(format!(
                "planned={}-{} but 206 response has no Content-Range header",
                plan.min_off,
                plan.max_end - 1
            ))
        })?;

    let (first, last) = parse_content_range(content_range).ok_or_else(|| {
        DrmError::RangeMismatch(format!(
            "planned={}-{} but Content-Range is not parseable: {content_range}",
            plan.min_off,
            plan.max_end - 1
        ))
    })?;

    if first != plan.min_off || last + 1 < plan.max_end {
        return Err(DrmError::RangeMismatch(format!(
            "planned={}-{} got={first}-{last}",
            plan.min_off,
            plan.max_end - 1
        )));
    }

    if let Some(len) = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        let expected = last - first + 1;
        if len != expected {
            return Err(DrmError::RangeMismatch(format!(
                "Content-Range window is {first}-{last} ({expected} bytes) but Content-Length is {len}"
            )));
        }
    }

    Ok(())
}

/// Parses an HTTP `Content-Range: bytes <first>-<last>/<total|*>` header's
/// `(first, last)` byte positions. Returns `None` on any shape it does not
/// recognize; `*` (unknown total) is tolerated — only the first and last
/// byte positions are required for the planner's check.
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let tail = value.trim().strip_prefix("bytes ")?;
    let (range, _total) = tail.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    Some((first, last))
}

/// Pure assembly step: slice each [`SegmentRange`] out of its fetched buffer
/// and concatenate the pieces in `refs` order. Isolated from HTTP so it can be
/// unit-tested without wire I/O — the primary regression guard against the
/// "download the whole file once per segment" bug.
fn assemble_from_fetched(
    refs: &[SegmentRange],
    fetched: &HashMap<String, FetchedUrl>,
) -> Result<Assembled, DrmError> {
    // Reserve for the ranged portions; whole-file pieces are a rare path and
    // the extend_from_slice below will grow the vec as needed.
    let hinted: usize = refs
        .iter()
        .filter_map(|r| r.byte_range.and_then(|(_, len)| usize::try_from(len).ok()))
        .sum();
    let mut out = Vec::with_capacity(hinted);
    let mut starts = Vec::with_capacity(refs.len());
    for r in refs {
        starts.push(out.len());
        let got = fetched.get(&r.url).ok_or_else(|| {
            DrmError::Api(format!("internal: no fetched buffer for url {}", r.url))
        })?;
        match r.byte_range {
            None => {
                if got.base_offset != 0 {
                    return Err(DrmError::Api(format!(
                        "internal: whole-file segment {} but fetched buffer starts at {}",
                        r.url, got.base_offset
                    )));
                }
                out.extend_from_slice(&got.bytes);
            }
            Some((offset, length)) => {
                let start = offset.checked_sub(got.base_offset).ok_or_else(|| {
                    DrmError::Api(format!(
                        "segment {} range @{offset} precedes fetched base @{}",
                        r.url, got.base_offset
                    ))
                })?;
                let end = start.saturating_add(length);
                let start_usize = usize::try_from(start)
                    .map_err(|_| DrmError::Api(format!("segment {} slice overflow", r.url)))?;
                let end_usize = usize::try_from(end)
                    .map_err(|_| DrmError::Api(format!("segment {} slice overflow", r.url)))?;
                if end_usize > got.bytes.len() {
                    return Err(DrmError::Api(format!(
                        "segment {} slice {start_usize}..{end_usize} out of bounds (fetched {} bytes from @{})",
                        r.url,
                        got.bytes.len(),
                        got.base_offset
                    )));
                }
                out.extend_from_slice(&got.bytes[start_usize..end_usize]);
            }
        }
    }
    Ok(Assembled { bytes: out, starts })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(url: &str, range: Option<(u64, u64)>) -> SegmentRange {
        SegmentRange {
            url: url.to_owned(),
            byte_range: range,
        }
    }

    /// Mirrors Apple's AAC CENC manifest shape: init + several segments all
    /// pointing at the same URL with disjoint byte ranges. The assembled
    /// output must equal the exact slices concatenated in playlist order,
    /// NOT the full file 21× over.
    #[test]
    fn assembled_size_equals_sum_of_ranges() {
        // Build a 10 000-byte "source file" with distinctive content so slice
        // boundaries are easy to verify.
        let full: Vec<u8> = (0..10_000u32).map(|i| (i & 0xFF) as u8).collect();
        let url = "https://cdn.example.com/a.mp4";
        // Init: bytes [0, 1006), then three segments abutting each other.
        let refs = vec![
            seg(url, Some((0, 1006))),
            seg(url, Some((1006, 2000))),
            seg(url, Some((3006, 500))),
            seg(url, Some((3506, 1500))),
        ];
        // Simulate a 206 Partial Content covering the union [0, 5006).
        let fetched = HashMap::from([(
            url.to_owned(),
            FetchedUrl {
                base_offset: 0,
                bytes: full[..5006].to_vec(),
            },
        )]);
        let out = assemble_from_fetched(&refs, &fetched).unwrap().bytes;
        let expected_len: u64 = refs
            .iter()
            .map(|r| r.byte_range.map_or(0, |(_, l)| l))
            .sum();
        assert_eq!(
            out.len() as u64,
            expected_len,
            "assembled size must equal sum of segment ranges, not full-file repeats"
        );
        // Bytes identity: each slice equals the matching region of the
        // original file.
        let mut cursor = 0usize;
        for r in &refs {
            let (off, len) = r.byte_range.unwrap();
            let off = usize::try_from(off).unwrap();
            let len = usize::try_from(len).unwrap();
            assert_eq!(&out[cursor..cursor + len], &full[off..off + len]);
            cursor += len;
        }
    }

    /// When the CDN replies `200 OK` to a `Range` request (fetched buffer
    /// starts at offset 0), the slicer must still pick out the requested
    /// sub-ranges from inside the full-file buffer.
    #[test]
    fn assemble_handles_full_file_response_to_range_request() {
        let full: Vec<u8> = (0..2000u32).map(|i| (i & 0xFF) as u8).collect();
        let url = "https://cdn.example.com/b.mp4";
        let refs = vec![seg(url, Some((100, 50))), seg(url, Some((1000, 200)))];
        // base_offset = 0 simulates a 200 OK (entire file returned).
        let fetched = HashMap::from([(
            url.to_owned(),
            FetchedUrl {
                base_offset: 0,
                bytes: full.clone(),
            },
        )]);
        let out = assemble_from_fetched(&refs, &fetched).unwrap().bytes;
        assert_eq!(out.len(), 250);
        assert_eq!(&out[..50], &full[100..150]);
        assert_eq!(&out[50..250], &full[1000..1200]);
    }

    /// 206 Partial Content: the fetched buffer's `base_offset` is non-zero,
    /// so the slicer must subtract it before indexing.
    #[test]
    fn assemble_handles_partial_content_with_nonzero_base() {
        let full: Vec<u8> = (0..1000u32).map(|i| (i & 0xFF) as u8).collect();
        let url = "https://cdn.example.com/c.mp4";
        let refs = vec![seg(url, Some((200, 100))), seg(url, Some((500, 50)))];
        // The CDN handed us only bytes [200, 550) via 206.
        let fetched = HashMap::from([(
            url.to_owned(),
            FetchedUrl {
                base_offset: 200,
                bytes: full[200..550].to_vec(),
            },
        )]);
        let out = assemble_from_fetched(&refs, &fetched).unwrap().bytes;
        assert_eq!(out.len(), 150);
        assert_eq!(&out[..100], &full[200..300]);
        assert_eq!(&out[100..150], &full[500..550]);
    }

    /// Smoke-test the glue that moves `cenc::decrypt` onto `spawn_blocking`:
    /// the `JoinHandle` must deliver the inner `Result` unchanged, rather than
    /// swallowing it into `DrmError::Join`. `decrypt` rejects any buffer that
    /// lacks a proper `moov/.../stsd/enca/sinf/schm` chain, so the test
    /// asserts that a typed `Err(Decrypt(..))` round-trips, which exercises
    /// the same glue path.
    #[test]
    fn spawn_blocking_decrypt_round_trips_result() {
        // Minimal non-CENC payload: one `ftyp` box with no body. `decrypt`
        // walks the structure, finds no `moov`, and returns a typed error.
        let data: Vec<u8> = vec![0, 0, 0, 8, b'f', b't', b'y', b'p'];
        let keys = ContentKeys::single([0u8; 16]);

        // A current-thread runtime is enough — the test only awaits the
        // single `JoinHandle` and exercises the error-mapping glue. No new
        // dev-dependency: the `rt` + `fs` features are already on tokio via
        // the main crate.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(async move {
            tokio::task::spawn_blocking(move || cenc::decrypt(data, &keys))
                .await
                .map_err(|e| DrmError::Join(e.to_string()))
        });
        // The outer `Result` (join) must be Ok (no `Join` error): the inner
        // `Result` carries the typed `Decrypt` error we want to see.
        let inner = out.expect("spawn_blocking join must not fail");
        match inner {
            Err(DrmError::Decrypt(_)) => {}
            other => {
                panic!("spawn_blocking must round-trip decrypt's Result unchanged; got {other:?}")
            }
        }
    }

    fn plan(min_off: u64, max_end: u64) -> UrlFetchPlan {
        UrlFetchPlan {
            fetch_full: false,
            min_off,
            max_end,
        }
    }

    #[test]
    fn parse_content_range_accepts_bytes_form() {
        assert_eq!(parse_content_range("bytes 100-199/5000"), Some((100, 199)));
        assert_eq!(parse_content_range("bytes 0-0/*"), Some((0, 0)));
    }

    #[test]
    fn parse_content_range_rejects_malformed_values() {
        assert_eq!(parse_content_range("items 0-9/10"), None);
        assert_eq!(parse_content_range("bytes 100/5000"), None);
        assert_eq!(parse_content_range("bytes -10/5000"), None);
    }

    #[test]
    fn validate_partial_range_accepts_matching_window() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 100-299/10000".parse().unwrap(),
        );
        headers.insert(reqwest::header::CONTENT_LENGTH, "200".parse().unwrap());
        assert!(validate_partial_range(&headers, plan(100, 300)).is_ok());
    }

    #[test]
    fn validate_partial_range_rejects_offset_mismatch() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 50-299/10000".parse().unwrap(),
        );
        let err = validate_partial_range(&headers, plan(100, 300)).unwrap_err();
        assert!(matches!(err, DrmError::RangeMismatch(_)));
    }

    #[test]
    fn validate_partial_range_rejects_short_window() {
        let mut headers = reqwest::header::HeaderMap::new();
        // last=249 means window ends at 250 but plan wants up to 300.
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 100-249/10000".parse().unwrap(),
        );
        let err = validate_partial_range(&headers, plan(100, 300)).unwrap_err();
        assert!(matches!(err, DrmError::RangeMismatch(_)));
    }

    #[test]
    fn validate_partial_range_rejects_mismatched_content_length() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 100-299/10000".parse().unwrap(),
        );
        // Window is 200 bytes, CL claims 300: disagreement must fail closed.
        headers.insert(reqwest::header::CONTENT_LENGTH, "300".parse().unwrap());
        let err = validate_partial_range(&headers, plan(100, 300)).unwrap_err();
        assert!(matches!(err, DrmError::RangeMismatch(_)));
    }

    #[test]
    fn validate_partial_range_missing_header_is_rejected() {
        let headers = reqwest::header::HeaderMap::new();
        let err = validate_partial_range(&headers, plan(100, 300)).unwrap_err();
        assert!(matches!(err, DrmError::RangeMismatch(_)));
    }

    /// Byte-progress accounting from the chunk loop: for each received chunk
    /// `done` must grow by the chunk's length and the reported `total` must
    /// stay `>= done` even when a server overshoots the planned union length.
    /// Exercises the exact `AtomicU64` pattern the per-URL tasks use, without
    /// standing up an HTTP mock.
    #[test]
    fn chunked_progress_done_never_exceeds_total() {
        let total = Arc::new(AtomicU64::new(1_000)); // planned union length
        let done = Arc::new(AtomicU64::new(0));

        let simulated_chunks = [400u64, 400, 400]; // last chunk pushes done past planned total
        let mut reports: Vec<(u64, u64)> = Vec::new();
        for n in simulated_chunks {
            let now = done.fetch_add(n, Ordering::Relaxed) + n;
            let total_now = total.load(Ordering::Relaxed).max(now);
            reports.push((now, total_now));
        }

        assert_eq!(
            reports,
            vec![(400, 1_000), (800, 1_000), (1_200, 1_200)],
            "`report(done, Some(total))` must always satisfy done <= total"
        );
    }

    // --- spatial key mapping -------------------------------------------------

    fn mk_box(ty: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + payload.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(payload);
        v
    }

    /// A `moof` whose `tfhd` names `index`, or relies on the default.
    fn mk_moof(index: Option<u32>) -> Vec<u8> {
        let tfhd = match index {
            Some(i) => mk_box(
                b"tfhd",
                &[&[0, 0, 0, 2][..], &1u32.to_be_bytes(), &i.to_be_bytes()].concat(),
            ),
            None => mk_box(b"tfhd", &[&[0, 0, 0, 0][..], &1u32.to_be_bytes()].concat()),
        };
        mk_box(b"moof", &mk_box(b"traf", &tfhd))
    }

    /// Init piece + three one-moof segments (default, 2, 2).
    fn assembled() -> Assembled {
        let pieces = [
            mk_box(b"ftyp", b"iso5"),
            mk_moof(None),
            mk_moof(Some(2)),
            mk_moof(Some(2)),
        ];
        let mut bytes = Vec::new();
        let mut starts = Vec::new();
        for piece in &pieces {
            starts.push(bytes.len());
            bytes.extend_from_slice(piece);
        }
        Assembled { bytes, starts }
    }

    fn group(segments: std::ops::Range<usize>) -> apple::KeyGroup {
        apple::KeyGroup {
            key_uri: format!("data:;base64,{}", segments.start),
            pssh: Vec::new(),
            segments,
        }
    }

    #[test]
    fn spatial_keys_should_map_each_sample_description_to_its_group_key() {
        let keys = spatial_keys(
            &assembled(),
            &[group(0..1), group(1..3)],
            &[[1; 16], [2; 16]],
        );
        assert_eq!(
            format!("{:?}", keys.unwrap()),
            "ContentKeys { single: false, sample_descriptions: [1, 2], .. }"
        );
    }

    #[test]
    fn spatial_keys_should_reject_one_description_with_two_keys() {
        let keys = spatial_keys(
            &assembled(),
            &[group(0..2), group(2..3)],
            &[[1; 16], [2; 16]],
        );
        assert!(matches!(keys, Err(DrmError::Decrypt(_))));
    }

    #[test]
    fn spatial_keys_should_reject_a_segment_outside_every_group() {
        let keys = spatial_keys(&assembled(), &[group(0..1)], &[[1; 16]]);
        assert!(matches!(keys, Err(DrmError::Decrypt(_))));
    }

    #[test]
    fn drm_downloader_should_declare_spatial_support() {
        let storage = Storage::open_in_memory().unwrap();
        let drm = DrmDownloader::with_default_device(storage).unwrap();
        assert!(drm.capabilities().spatial);
    }
}
