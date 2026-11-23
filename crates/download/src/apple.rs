//! Apple Music playback API and HLS manifest parsing.
//!
//! Browser-shaped headers are applied to every request (`webPlayback`,
//! `widevineCert`, `acquireWebPlaybackLicense`). Apple hands Widevine (CTR)
//! assets only when the request looks like a Chromium desktop browser, so
//! the headers mimic one.

use std::collections::HashMap;
use std::time::Duration;

use base64::prelude::*;
use cider_offline_core::download::{MediaKind, PlaybackId};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;

use crate::error::DrmError;

/// Checks a `reqwest::Response`'s status BEFORE its body is consumed, so
/// `Retry-After` and other diagnostic headers survive the move. Returns
/// [`DrmError::HttpStatus`] for any non-2xx response; the body length is
/// provided by the caller (who has already read the body).
///
/// Call this at every `play.itunes.apple.com` / CDN response in the drm crate.
fn check(
    endpoint: &'static str,
    resp: &reqwest::Response,
    body_len: usize,
) -> Result<(), DrmError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    Err(DrmError::HttpStatus {
        endpoint,
        status: status.as_u16(),
        body_len,
        retry_after,
    })
}

/// Production webPlayback endpoint. Tests override this via
/// [`Apple::with_web_playback`] so wiremock can serve canned responses.
pub(crate) const PLAYBACK_URL: &str =
    "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback";
/// The license endpoint for enhancedHls (spatial) keys: the fixed URL every
/// webPlayback answer names as `hls-key-server-url`. A spatial download makes
/// no webPlayback call, so it has no per-song URL to take it from.
pub(crate) const LICENSE_URL: &str =
    "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense";
/// `AUDIO` group of the spatial stream in an enhancedHls master playlist:
/// the Dolby Atmos mix rendered to binaural stereo, AAC-LC 256 kbps.
const BINAURAL_AUDIO_GROUP: &str = "audio-stereo-256-binaural";
const ORIGIN_VALUE: &str = "https://music.apple.com";
const REFERER_VALUE: &str = "https://music.apple.com/";
/// Windows Chrome 148 user-agent string, matched byte-for-byte with Cider's
/// captured traffic so Apple returns the Widevine (CTR) asset flavor.
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/148.0.0.0 Safari/537.36";
/// Preferred Widevine flavor (CTR, 256 kbps VBR AAC).
const FLAVOR_CTRP256: &str = ":ctrp256";
/// Fallback Widevine flavor (CTR, 64 kbps AAC).
const FLAVOR_CTRP64: &str = ":ctrp64";
/// Widevine KEYFORMAT uuid used in `#EXT-X-KEY`.
const WIDEVINE_KEYFORMAT_UUID: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Builds the browser-shaped auth header set used by every endpoint in this
/// module.
///
/// The two token headers are flagged `sensitive` so their values never appear
/// in `reqwest`'s debug logs.
fn build_headers(auth_token: &str, media_user_token: &str) -> Result<HeaderMap, DrmError> {
    let mut headers = HeaderMap::with_capacity(8);

    let mut auth = HeaderValue::from_str(auth_token)
        .map_err(|e| DrmError::Api(format!("invalid developer token: {e}")))?;
    auth.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, auth);

    let mut mut_hdr = HeaderValue::from_str(media_user_token)
        .map_err(|e| DrmError::Api(format!("invalid media-user-token: {e}")))?;
    mut_hdr.set_sensitive(true);
    headers.insert(HeaderName::from_static("x-apple-music-user-token"), mut_hdr);

    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        reqwest::header::ORIGIN,
        HeaderValue::from_static(ORIGIN_VALUE),
    );
    headers.insert(
        reqwest::header::REFERER,
        HeaderValue::from_static(REFERER_VALUE),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

// ---------------------------------------------------------------------------
// Apple grouping: reqwest client + auth headers + webPlayback URL
// ---------------------------------------------------------------------------

/// The three pieces the Apple Music endpoints share for one download attempt:
/// the shared `reqwest::Client`, the auth-bearing `HeaderMap` (developer
/// token + media-user token + browser-shaped Chrome headers), and the
/// webPlayback endpoint URL. The `widevineCert` and `acquireWebPlaybackLicense`
/// URLs vary per song and come back on the `webPlayback` response, so they
/// are not held on this struct — each method takes its per-song URL as a
/// parameter.
///
/// Introduced so wiremock tests can point the whole flow at a local
/// mock server via [`Self::with_web_playback`].
pub(crate) struct Apple {
    client: reqwest::Client,
    headers: HeaderMap,
    web_playback: String,
}

impl Apple {
    /// Builds an [`Apple`] from a shared `reqwest::Client` and the two Apple
    /// Music auth tokens. Uses the production `webPlayback` URL by default;
    /// tests override it with [`Self::with_web_playback`].
    ///
    /// # Errors
    ///
    /// Returns [`DrmError::Api`] if either token is not a valid HTTP header
    /// value (contains CR/LF / NUL / other control bytes).
    pub fn new(
        client: reqwest::Client,
        auth_token: &str,
        media_user_token: &str,
    ) -> Result<Self, DrmError> {
        Ok(Self {
            client,
            headers: build_headers(auth_token, media_user_token)?,
            web_playback: PLAYBACK_URL.to_owned(),
        })
    }

    /// Overrides the webPlayback URL (e.g. to a wiremock server base URL).
    #[cfg(test)]
    #[must_use]
    pub fn with_web_playback(mut self, url: impl Into<String>) -> Self {
        self.web_playback = url.into();
        self
    }
}

// ---------------------------------------------------------------------------
// webPlayback API
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PlaybackResponse {
    #[serde(rename = "songList")]
    song_list: Option<Vec<SongEntry>>,
    #[serde(rename = "failureType")]
    failure_type: Option<String>,
    #[serde(rename = "customerMessage")]
    customer_message: Option<String>,
    dialog: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SongEntry {
    assets: Option<Vec<Asset>>,
    #[serde(rename = "widevine-cert-url")]
    widevine_cert_url: Option<String>,
    #[serde(rename = "hls-key-server-url")]
    hls_key_server_url: Option<String>,
    /// Music-video `webPlayback` responses carry `assets: []` and the audio-plus-video
    /// master playlist URL here instead. Songs leave this absent.
    #[serde(rename = "hls-playlist-url")]
    hls_playlist_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub flavor: String,
    #[serde(rename = "URL")]
    pub url: String,
}

/// Which HLS manifest a successful `webPlayback` call pointed the downloader at.
///
/// Songs carry a chosen `ctrp*` song asset; music videos carry only the
/// account-specific master HLS playlist URL (`hls-playlist-url`) which the
/// downloader then walks to the audio rendition itself.
#[derive(Debug, Clone)]
pub enum Manifest {
    /// Song flow: one of the `ctrp*` asset URLs from `songList[0].assets`.
    SongAsset {
        /// The chosen asset's HLS manifest URL.
        url: String,
        /// The chosen asset's `flavor` string (e.g. `28:ctrp256`). Passed into
        /// the HLS fetch layer so the diagnostic dump can include it when the
        /// key selector fails.
        flavor: String,
    },
    /// Music-video flow: the `hls-playlist-url` master playlist pointing at both
    /// video variants and audio renditions. The downloader picks an audio
    /// rendition from `#EXT-X-MEDIA` and fetches its media playlist.
    VideoMaster(String),
}

/// What a successful `webPlayback` call tells the downloader.
#[derive(Debug, Clone)]
pub struct WidevinePlayback {
    /// Which manifest to follow (song asset vs music-video master playlist).
    pub manifest: Manifest,
    /// Song-level `widevine-cert-url`: GET returns the Widevine service cert.
    pub cert_url: String,
    /// Song-level `hls-key-server-url`: POST the challenge here.
    pub license_url: String,
}

impl Apple {
    /// Issues `POST webPlayback` and returns the Widevine asset + endpoint URLs.
    ///
    /// # Errors
    ///
    /// - [`DrmError::SessionExpired`] when Apple signals stale tokens.
    /// - [`DrmError::HttpStatus`] when Apple returns a non-success status.
    /// - [`DrmError::EmptySongList`] when the response has an empty `songList`.
    /// - [`DrmError::NoAudioAsset`] when no `ctrp*` flavor is present.
    /// - [`DrmError::MissingCertUrl`] when `widevine-cert-url` is absent.
    pub async fn fetch_playback(
        &self,
        playback: &PlaybackId,
        kind: MediaKind,
    ) -> Result<WidevinePlayback, DrmError> {
        let body = playback.web_playback_body();

        let resp = self
            .client
            .post(&self.web_playback)
            .headers(self.headers.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .json(&body)
            .send()
            .await?;
        // `check` reads status + `Retry-After` from `&resp` BEFORE `.text()`
        // consumes the response. body_len is unknown at this point; the status
        // is the load-bearing field for the queue's retry decision.
        check("webPlayback", &resp, 0)?;
        let raw = resp.text().await?;

        // Try to detect session-expired BEFORE parsing — the shape may differ.
        if raw.contains("\"failureType\":\"2002\"") || raw.contains("Your session has ended") {
            return Err(DrmError::SessionExpired);
        }

        let resp: PlaybackResponse = serde_json::from_str(&raw)
            .map_err(|e| DrmError::Api(format!("webPlayback bad JSON: {e}")))?;

        if resp.failure_type.as_deref() == Some("2002") {
            return Err(DrmError::SessionExpired);
        }
        if let Some(msg) = &resp.customer_message
            && msg.contains("Your session has ended")
        {
            return Err(DrmError::SessionExpired);
        }
        if let Some(dialog) = resp.dialog {
            return Err(DrmError::Api(format!("webPlayback dialog: {dialog}")));
        }

        let adam_id = playback.adam_id().to_owned();
        let song = resp
            .song_list
            .and_then(|mut songs| (!songs.is_empty()).then(|| songs.remove(0)))
            .ok_or_else(|| DrmError::EmptySongList(adam_id.clone()))?;

        let assets = song.assets.unwrap_or_default();
        let cert_url = song.widevine_cert_url.ok_or(DrmError::MissingCertUrl)?;
        let license_url = song.hls_key_server_url.ok_or_else(|| {
            DrmError::Api("webPlayback response missing hls-key-server-url".into())
        })?;

        // Music-video flow: songList[0] has `assets: []` plus `hls-playlist-url`.
        // Core only sends MediaKind::MusicVideo when we declare it; we still gate on
        // `assets.is_empty()` so a song that happened to also carry `hls-playlist-url`
        // (not observed in Cider's captures) stays on the song path.
        if kind == MediaKind::MusicVideo && assets.is_empty() {
            let url = song.hls_playlist_url.ok_or_else(|| {
                DrmError::NoAudioAsset("music video: hls-playlist-url missing".into())
            })?;
            return Ok(WidevinePlayback {
                manifest: Manifest::VideoMaster(url),
                cert_url,
                license_url,
            });
        }

        let asset = pick_widevine_asset(&assets).ok_or_else(|| {
            if assets.is_empty() {
                DrmError::NoAudioAsset("assets: []".into())
            } else {
                let flavors: Vec<&str> = assets.iter().map(|a| a.flavor.as_str()).collect();
                DrmError::NoAudioAsset(format!(
                    "no ctrp asset among flavors: {}",
                    flavors.join(", ")
                ))
            }
        })?;

        Ok(WidevinePlayback {
            manifest: Manifest::SongAsset {
                url: asset.url.clone(),
                flavor: asset.flavor.clone(),
            },
            cert_url,
            license_url,
        })
    }
}

/// Prefers `…:ctrp256`, falls back to `…:ctrp64`.
fn pick_widevine_asset(assets: &[Asset]) -> Option<&Asset> {
    assets
        .iter()
        .find(|a| a.flavor.ends_with(FLAVOR_CTRP256))
        .or_else(|| assets.iter().find(|a| a.flavor.ends_with(FLAVOR_CTRP64)))
}

// ---------------------------------------------------------------------------
// Widevine service certificate
// ---------------------------------------------------------------------------

impl Apple {
    /// Fetches the binary Widevine service certificate from the per-song
    /// `widevineCert` URL returned by [`Self::fetch_playback`].
    ///
    /// # Errors
    ///
    /// - [`DrmError::HttpStatus`] when Apple returns a non-success status.
    /// - [`DrmError::Http`] on transport error.
    pub async fn fetch_widevine_cert(&self, url: &str) -> Result<Vec<u8>, DrmError> {
        let resp = self
            .client
            .get(url)
            .headers(self.headers.clone())
            .send()
            .await?;
        check("widevineCert", &resp, 0)?;
        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }
}

// ---------------------------------------------------------------------------
// License API
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LicenseResponse {
    license: Option<String>,
    #[serde(rename = "errorCode")]
    error_code: Option<i64>,
    status: Option<i64>,
    #[serde(rename = "renew-after")]
    renew_after: Option<serde_json::Value>,
    message: Option<String>,
}

impl Apple {
    /// POSTs to `acquireWebPlaybackLicense` with the exact JSON shape Apple
    /// returns in capture `…-000052`. One request, one result — core retries.
    ///
    /// # Errors
    ///
    /// - [`DrmError::HttpStatus`] on non-2xx.
    /// - [`DrmError::EmptyLicense`] when the body is non-JSON, missing a
    ///   license string, or carries a server status/error code.
    /// - [`DrmError::SessionExpired`] when `errorCode=2002` surfaces.
    #[expect(
        clippy::too_many_lines,
        reason = "diagnostic logging (challenge preview + response dump) plus the error-mapping cascade reads best as one top-to-bottom function"
    )]
    pub async fn acquire_license(
        &self,
        license_url: &str,
        challenge_b64: &str,
        key_uri: &str,
        playback: &PlaybackId,
    ) -> Result<Vec<u8>, DrmError> {
        // One-time diagnostic: dump the first bytes of the outgoing challenge so
        // the SignedMessage msg_type + wire start are visible in the logs.
        // The first byte of a SignedMessage protobuf is 0x08 (tag=1 varint,
        // `type`), the second is 0x01 (msg_type = LICENSE_REQUEST). Anything
        // else here would mean the CDM built the wrong envelope.
        if let Ok(decoded) = BASE64_STANDARD.decode(challenge_b64) {
            use std::fmt::Write;
            let head_hex: String =
                decoded
                    .iter()
                    .take(16)
                    .fold(String::with_capacity(32), |mut s, b| {
                        let _ = write!(s, "{b:02x}");
                        s
                    });
            tracing::debug!(
                challenge_bytes_len = decoded.len(),
                challenge_head_hex = %head_hex,
                key_uri_len = key_uri.len(),
                "acquire_license: outgoing challenge (first 16 bytes of base64-decoded SignedMessage)"
            );
        }

        let is_library = playback.is_library();
        let payload = serde_json::json!({
            "challenge": challenge_b64,
            "uri": key_uri,
            "key-system": "com.widevine.alpha",
            "adamId": playback.adam_id(),
            "isLibrary": is_library,
            "user-initiated": true,
        });

        let http_resp = self
            .client
            .post(license_url)
            .headers(self.headers.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .json(&payload)
            .send()
            .await?;
        // Status check first — a 401/403/429/5xx becomes a typed HttpStatus
        // error with Retry-After, so core retries correctly.
        check("license", &http_resp, 0)?;
        let status = http_resp.status();
        let text = http_resp.text().await?;

        // A 2xx with an empty or non-JSON body is Apple's "overloaded"
        // signal: transient, not permanent. Classified distinctly from
        // HttpStatus so the From<DrmError> mapping can keep EmptyLicense -> 200.
        let resp: LicenseResponse = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => {
                return Err(DrmError::EmptyLicense {
                    status: status.as_u16(),
                    body_len: text.len(),
                });
            }
        };

        let err_code = resp.error_code.unwrap_or(0);
        let status_code = resp.status.unwrap_or(0);
        let license_b64_len = resp.license.as_deref().map_or(0, str::len);

        // Decoded license length + head hex are only useful if the body is
        // present. The log is at `debug` so it costs nothing in production and
        // immediately tells an operator (a) whether Apple handed back a license
        // at all, and (b) whether the first bytes look like a Widevine
        // `SignedMessage` protobuf (field-1 tag byte = 0x08 for an int32 type).
        let (decoded_len, decoded_head_hex): (usize, String) = match resp.license.as_deref() {
            Some(b64) if !b64.is_empty() => match BASE64_STANDARD.decode(b64) {
                Ok(bytes) => {
                    use std::fmt::Write;
                    let hex: String =
                        bytes
                            .iter()
                            .take(8)
                            .fold(String::with_capacity(16), |mut s, b| {
                                let _ = write!(s, "{b:02x}");
                                s
                            });
                    (bytes.len(), hex)
                }
                Err(_) => (0, String::new()),
            },
            _ => (0, String::new()),
        };

        tracing::debug!(
            http_status = status.as_u16(),
            body_len = text.len(),
            is_library,
            license_b64_len,
            decoded_license_len = decoded_len,
            decoded_license_head_hex = %decoded_head_hex,
            error_code = err_code,
            status_code,
            renew_after_present = resp.renew_after.is_some(),
            message = resp.message.as_deref().unwrap_or(""),
            "acquire_license response"
        );

        // errorCode=2002 ("Your session has ended") is a session-expired
        // signal from the license server — surface it as SessionExpired so
        // core's From<DrmError> mapping flips it to AuthExpired.
        if err_code == 2002 || status_code == 2002 {
            return Err(DrmError::SessionExpired);
        }

        if err_code != 0 || status_code != 0 {
            return Err(DrmError::License(format!(
                "errorCode={err_code} status={status_code}"
            )));
        }

        if let Some(license_b64) = resp.license.filter(|s| !s.is_empty()) {
            return BASE64_STANDARD
                .decode(&license_b64)
                .map_err(|e| DrmError::License(format!("bad base64 license: {e}")));
        }

        // 200 OK, JSON parsed, but no license string: treat as an empty license.
        Err(DrmError::EmptyLicense {
            status: status.as_u16(),
            body_len: text.len(),
        })
    }
}

// ---------------------------------------------------------------------------
// HLS manifest parsing
// ---------------------------------------------------------------------------

/// One piece to fetch from a (possibly shared) media resource: a URL plus an
/// optional resolved absolute byte range.
///
/// When `byte_range` is `Some((offset, length))` the downloader fetches those
/// bytes only (via a `Range` request). When it is `None` the entire resource
/// at `url` is fetched. Multiple [`SegmentRange`]s may point at the same URL
/// — see [`HlsInfo`] for how Apple's live AAC CENC manifests use this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRange {
    /// Absolute URL of the media resource.
    pub url: String,
    /// Resolved absolute `(offset, length)` from `#EXT-X-BYTERANGE` or
    /// `#EXT-X-MAP:BYTERANGE`. `None` means "fetch the entire resource".
    pub byte_range: Option<(u64, u64)>,
}

/// What the HLS manifest tells the downloader.
#[derive(Debug, Clone)]
pub struct HlsInfo {
    /// Widevine PSSH init-data bytes derived from the `#EXT-X-KEY` data URI.
    ///
    /// Apple's real AAC CENC manifests encode a bare 16-byte `default_KID` in
    /// the URI; the selector wraps that KID into a Widevine v0 PSSH box here.
    /// If an upstream manifest ever embeds a full PSSH box in the URI (e.g.
    /// other services or a future Apple variant) it is preserved verbatim.
    /// Passed straight through to [`crate::cdm::Cdm::open_session_with_cert`].
    pub pssh_init_data: Vec<u8>,
    /// The `#EXT-X-KEY` `URI="data:…;base64,…"` value, verbatim. The license
    /// request body includes this exact string under `uri`.
    pub key_uri: String,
    /// `#EXT-X-MAP` init segment — URL and optional `BYTERANGE` slice.
    pub init_segment: SegmentRange,
    /// Ordered media segments. In Apple's AAC CENC manifests every entry
    /// points at the same URL with disjoint byte ranges; in general each
    /// segment may have its own URL and optional range.
    pub segments: Vec<SegmentRange>,
}

/// One Widevine key of a spatial media playlist and the segments it
/// encrypts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyGroup {
    /// The `#EXT-X-KEY` `URI="data:…"` value, verbatim (the license `uri`).
    pub key_uri: String,
    /// The Widevine PSSH box decoded from that URI.
    pub pssh: Vec<u8>,
    /// Indices into [`SpatialHls::segments`] this key encrypts.
    pub segments: std::ops::Range<usize>,
}

/// What a spatial (enhancedHls binaural) media playlist tells the
/// downloader.
#[derive(Debug, Clone)]
pub struct SpatialHls {
    /// `#EXT-X-MAP` init segment.
    pub init_segment: SegmentRange,
    /// Ordered media segments.
    pub segments: Vec<SegmentRange>,
    /// The Widevine keys in playlist order; together they cover every
    /// segment exactly once.
    pub key_groups: Vec<KeyGroup>,
}

/// Fetches the enhancedHls master at `master_url`, picks the binaural
/// (spatial) variant and parses its media playlist. `Ok(None)` when the
/// master has no binaural variant (the song has no Dolby Atmos mix).
///
/// Both playlists are public CDN files: sent with browser-only headers, no
/// tokens.
pub async fn fetch_spatial_hls(
    client: &reqwest::Client,
    master_url: &str,
) -> Result<Option<SpatialHls>, DrmError> {
    let headers = minimal_browser_headers();
    let resp = client
        .get(master_url)
        .headers(headers.clone())
        .send()
        .await?;
    check("playlist", &resp, 0)?;
    let master = resp.text().await?;
    let Some(variant) = pick_binaural_variant(&master) else {
        return Ok(None);
    };
    let media_url = resolve_url(master_url, variant)?;
    let resp = client.get(&media_url).headers(headers).send().await?;
    check("playlist", &resp, 0)?;
    let media = resp.text().await?;
    parse_spatial_playlist(&media, &media_url).map(Some)
}

/// The URI of the `#EXT-X-STREAM-INF` whose `AUDIO` group is
/// [`BINAURAL_AUDIO_GROUP`]: the line after the tag.
fn pick_binaural_variant(master: &str) -> Option<&str> {
    let mut lines = master.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        if parse_attr_list(attrs).get("AUDIO").map(String::as_str) != Some(BINAURAL_AUDIO_GROUP) {
            continue;
        }
        return lines.find(|l| !l.is_empty() && !l.starts_with('#'));
    }
    None
}

/// Parses a spatial media playlist: the segment layout (as for any media
/// playlist) plus its Widevine key groups.
///
/// A Widevine `#EXT-X-KEY` (by `KEYFORMAT`) applies to the segments that
/// follow it until the next one. `m3u8-rs` keeps only one key per segment,
/// so the keys are read from the raw text, counting segment URI lines.
fn parse_spatial_playlist(manifest: &str, base_url: &str) -> Result<SpatialHls, DrmError> {
    let (init_segment, segments) = parse_segment_layout(manifest, base_url)?;
    let key_groups = parse_key_groups(manifest, segments.len())?;
    Ok(SpatialHls {
        init_segment,
        segments,
        key_groups,
    })
}

/// The Widevine key groups of a media playlist with `segment_count`
/// segments.
///
/// # Errors
///
/// [`DrmError::MissingKeyUri`] when a segment precedes every Widevine key;
/// [`DrmError::HlsParse`] when the raw segment count disagrees with the
/// parsed one or a key URI is not base64.
fn parse_key_groups(manifest: &str, segment_count: usize) -> Result<Vec<KeyGroup>, DrmError> {
    let mut groups: Vec<KeyGroup> = Vec::new();
    let mut segment = 0usize;
    for line in manifest.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_attr_list(attrs);
            let is_widevine = attrs
                .get("KEYFORMAT")
                .is_some_and(|kf| kf.eq_ignore_ascii_case(WIDEVINE_KEYFORMAT_UUID));
            let Some(uri) = attrs.get("URI").filter(|_| is_widevine) else {
                continue;
            };
            let (_, b64) = uri
                .split_once("base64,")
                .ok_or_else(|| DrmError::HlsParse("Widevine key URI is not base64 data".into()))?;
            let decoded = BASE64_STANDARD
                .decode(b64)
                .map_err(|e| DrmError::HlsParse(format!("bad key data: {e}")))?;
            // A key line directly followed by another owns no segments.
            if groups.last().is_some_and(|g| g.segments.is_empty()) {
                groups.pop();
            }
            groups.push(KeyGroup {
                key_uri: uri.clone(),
                pssh: ensure_pssh_bytes(&decoded),
                segments: segment..segment,
            });
        } else if !line.is_empty() && !line.starts_with('#') {
            let group = groups.last_mut().ok_or(DrmError::MissingKeyUri)?;
            segment += 1;
            group.segments.end = segment;
        }
    }
    if groups.last().is_some_and(|g| g.segments.is_empty()) {
        groups.pop();
    }
    if segment != segment_count {
        return Err(DrmError::HlsParse(format!(
            "counted {segment} segment URIs but parsed {segment_count} segments"
        )));
    }
    if groups.is_empty() {
        return Err(DrmError::MissingKeyUri);
    }
    Ok(groups)
}

/// Fetches the HLS manifest at `m3u8_url`. If it is a master playlist, follows
/// the first `#EXT-X-STREAM-INF` into the media playlist before returning.
///
/// `flavor` is the asset flavor string from the `webPlayback` response (e.g.
/// `28:ctrp256`); it is only used to label the diagnostic dump emitted when
/// the Widevine key selector fails.
pub async fn fetch_hls_info(
    client: &reqwest::Client,
    m3u8_url: &str,
    flavor: &str,
) -> Result<HlsInfo, DrmError> {
    let headers = minimal_browser_headers();
    let resp = client.get(m3u8_url).headers(headers.clone()).send().await?;
    // Short-circuit 4xx/5xx with a typed HttpStatus (keeps Retry-After); if
    // 2xx but malformed HLS the parser's diagnostic dump still fires below.
    check("playlist", &resp, 0)?;
    let http_status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let text = resp.text().await?;

    let mut ctx = ManifestContext {
        asset_url: m3u8_url.to_owned(),
        flavor: flavor.to_owned(),
        http_status,
        content_type: content_type.clone(),
    };

    let parsed = match m3u8_rs::parse_playlist_res(text.as_bytes()) {
        Ok(p) => p,
        Err(e) => {
            // Not valid HLS at all — could be an HTML error page, a
            // redirect, or an unexpected payload. Dump before failing.
            dump_manifest_diagnostic(&ctx, "ParseError", &text, Some(&e.to_string()));
            return Err(DrmError::ManifestParseFailure(format!(
                "HLS manifest could not be parsed as HLS (status={}, content-type={}); see WARN log for body",
                ctx.http_status, ctx.content_type
            )));
        }
    };

    let (media_text, base_url) = match parsed {
        m3u8_rs::Playlist::MediaPlaylist(_) => (text, m3u8_url.to_owned()),
        m3u8_rs::Playlist::MasterPlaylist(master) => {
            let variant = master
                .variants
                .into_iter()
                .next()
                .ok_or_else(|| DrmError::HlsParse("master playlist has no variants".into()))?;
            let media_url = resolve_url(m3u8_url, &variant.uri)?;
            let resp = client.get(&media_url).headers(headers).send().await?;
            check("playlist", &resp, 0)?;
            let status = resp.status().as_u16();
            let media_content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let media_text = resp.text().await?;
            // The dump wants the ACTUAL manifest the selector saw.
            ctx.asset_url = media_url.clone();
            ctx.http_status = status;
            ctx.content_type = media_content_type;
            (media_text, media_url)
        }
    };

    parse_media_playlist(&media_text, &base_url, &ctx)
}

/// Fetches the music-video audio rendition.
///
/// `master_url` is the account-bound `hls-playlist-url` from webPlayback — it
/// already carries `cc`, `a`, `dsid`, `id`, `aec=HD` and is sent verbatim with
/// browser-only headers (no `Authorization`, no `x-apple-music-user-token`).
/// The master playlist's `#EXT-X-MEDIA:TYPE=AUDIO` entries are scanned for the
/// best-quality AAC-LC rendition (`audio-stereo-256` → `…-128` → `…-64`); its
/// audio-only media playlist is then parsed like any other Widevine HLS media
/// playlist (see the `KING` music-video regression tests below).
pub async fn fetch_video_hls_info(
    client: &reqwest::Client,
    master_url: &str,
) -> Result<HlsInfo, DrmError> {
    let headers = minimal_browser_headers();
    let resp = client
        .get(master_url)
        .headers(headers.clone())
        .send()
        .await?;
    check("playlist", &resp, 0)?;
    let http_status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let text = resp.text().await?;

    // Parse as a master playlist and pick the audio rendition.
    let master = m3u8_rs::parse_master_playlist_res(text.as_bytes()).map_err(|e| {
        DrmError::ManifestParseFailure(format!(
            "music-video master playlist parse failed (status={http_status}, content-type={content_type}): {e}"
        ))
    })?;
    let audio_rendition_uri = pick_audio(&master).ok_or_else(|| {
        DrmError::NoAudioAsset(
            "music-video master playlist has no audio-stereo-64/128/256 rendition".into(),
        )
    })?;
    let audio_playlist_url = resolve_url(master_url, audio_rendition_uri)?;

    let media_resp = client
        .get(&audio_playlist_url)
        .headers(headers.clone())
        .send()
        .await?;
    check("playlist", &media_resp, 0)?;
    let media_status = media_resp.status().as_u16();
    let media_content_type = media_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let media_text = media_resp.text().await?;

    let ctx = ManifestContext {
        asset_url: audio_playlist_url.clone(),
        flavor: "music-video-audio".into(),
        http_status: media_status,
        content_type: media_content_type,
    };
    parse_media_playlist(&media_text, &audio_playlist_url, &ctx)
}

/// Picks the best-quality AAC-LC audio rendition from a master playlist.
///
/// Prefers `audio-stereo-256` → `audio-stereo-128` → `audio-stereo-64` and
/// skips `audio-HE2-stereo-32` (HE-AAC v2). Returns the raw rendition URI
/// (absolute `mvod` URL carrying `accessKey`); the caller resolves it against
/// the master URL and sends the GET with browser-only headers.
fn pick_audio(master: &m3u8_rs::MasterPlaylist) -> Option<&str> {
    for group in ["audio-stereo-256", "audio-stereo-128", "audio-stereo-64"] {
        if let Some(uri) = master
            .alternatives
            .iter()
            .find(|m| m.media_type == m3u8_rs::AlternativeMediaType::Audio && m.group_id == group)
            .and_then(|m| m.uri.as_deref())
        {
            return Some(uri);
        }
    }
    None
}

/// Minimal browser-ish headers for CDN requests (manifest + segments). No
/// Authorization — the aod-ssl CDN rejects it.
fn minimal_browser_headers() -> HeaderMap {
    let mut h = HeaderMap::with_capacity(2);
    h.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    h.insert(
        reqwest::header::ORIGIN,
        HeaderValue::from_static(ORIGIN_VALUE),
    );
    h
}

/// Public helper so the orchestrator can share a header set for segment GETs.
pub fn cdn_headers() -> HeaderMap {
    minimal_browser_headers()
}

/// Context flowed from the fetch site to the key-selector site so the
/// diagnostic dump can label itself (asset URL, flavor, HTTP status,
/// content-type). An empty `asset_url` suppresses the dump — tests build this
/// via `Default::default()` to keep logs quiet.
#[derive(Debug, Clone, Default)]
struct ManifestContext {
    asset_url: String,
    flavor: String,
    http_status: u16,
    content_type: String,
}

fn parse_media_playlist(
    manifest: &str,
    base_url: &str,
    ctx: &ManifestContext,
) -> Result<HlsInfo, DrmError> {
    let (init_segment, segments) = parse_segment_layout(manifest, base_url)?;

    // m3u8-rs keeps only one Key per segment (the most-recently-seen tag), so
    // when Apple emits several `#EXT-X-KEY` lines with different KEYFORMATs
    // only the last wins. Scan the raw text for the Widevine key to be safe.
    let (pssh_init_data, key_uri) = find_widevine_key(manifest, ctx)?;

    Ok(HlsInfo {
        pssh_init_data,
        key_uri,
        init_segment,
        segments,
    })
}

/// The init segment and the ordered media segments of a media playlist,
/// with every byte range resolved to an absolute `(offset, length)`.
fn parse_segment_layout(
    manifest: &str,
    base_url: &str,
) -> Result<(SegmentRange, Vec<SegmentRange>), DrmError> {
    let playlist = m3u8_rs::parse_media_playlist_res(manifest.as_bytes())
        .map_err(|e| DrmError::HlsParse(e.to_string()))?;

    if playlist.segments.is_empty() {
        return Err(DrmError::MissingSegments);
    }

    // `EXT-X-MAP` lives on the first segment that has one. All segments in a
    // single media playlist share the same init.
    let map = playlist
        .segments
        .iter()
        .find_map(|s| s.map.as_ref())
        .ok_or(DrmError::MissingInitSegment)?;
    let init_url = resolve_url(base_url, &map.uri)?;

    // RFC 8216 §4.3.2.2: `#EXT-X-BYTERANGE:<n>[@<o>]`. When `@o` is omitted,
    // the sub-range begins immediately after the previous `#EXT-X-BYTERANGE`
    // on the same URL. The running end resets when the URL changes.
    let mut last_end_by_url: HashMap<String, u64> = HashMap::new();
    let resolve_range =
        |br: &m3u8_rs::ByteRange, url: &str, last_end: &mut HashMap<String, u64>| -> (u64, u64) {
            let offset = br
                .offset
                .unwrap_or_else(|| last_end.get(url).copied().unwrap_or(0));
            last_end.insert(url.to_owned(), offset.saturating_add(br.length));
            (offset, br.length)
        };

    let init_byte_range = map
        .byte_range
        .as_ref()
        .map(|br| resolve_range(br, &init_url, &mut last_end_by_url));
    let init_segment = SegmentRange {
        url: init_url,
        byte_range: init_byte_range,
    };

    let mut segments: Vec<SegmentRange> = Vec::with_capacity(playlist.segments.len());
    for seg in &playlist.segments {
        let url = resolve_url(base_url, &seg.uri)?;
        let byte_range = seg
            .byte_range
            .as_ref()
            .map(|br| resolve_range(br, &url, &mut last_end_by_url));
        segments.push(SegmentRange { url, byte_range });
    }

    Ok((init_segment, segments))
}

/// Priority rank for a Widevine `#EXT-X-KEY` match. Higher wins.
///
/// Apple's real AAC CENC manifests ship `METHOD=ISO-23001-7` with **no**
/// `KEYFORMAT` attribute; other services (and Apple's older variants) have
/// used `KEYFORMAT=urn:uuid:edef8ba9-…` or `METHOD=SAMPLE-AES-CTR`. The
/// selector accepts any of them and prefers the most explicit signal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum KeyMatchRank {
    SampleAesCtr,
    Iso23001_7,
    WidevineKeyformat,
}

/// Extracts the Widevine `#EXT-X-KEY` `URI="data:…"` value plus the Widevine
/// PSSH init-data bytes.
///
/// Match criterion (any of the following; most specific wins):
/// - `KEYFORMAT` equals the Widevine system uuid
///   (`urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed`, case-insensitive), OR
/// - `METHOD=ISO-23001-7` (case-insensitive), OR
/// - `METHOD=SAMPLE-AES-CTR` (case-insensitive).
///
/// Neither attribute is required; a line with both absent is skipped. When
/// several candidates exist on the same scope the priority is
/// Widevine-`KEYFORMAT` > `ISO-23001-7` > `SAMPLE-AES-CTR`; ties go to the
/// first line in the file.
///
/// Fallback order (segment-level first, session-level second): per RFC 8216
/// §4.3.4.5 a segment-level `#EXT-X-KEY` overrides any session-level
/// `#EXT-X-SESSION-KEY` for the segments that follow it, so the segment-level
/// value is the authoritative one when both are present. We only fall back to
/// `#EXT-X-SESSION-KEY` when no matching segment-level key was found.
///
/// On failure the full diagnostic (asset url, flavor, http status,
/// content-type, body preview, raw key/session-key lines, total line counts)
/// is dumped via `tracing::warn!` before `MissingKeyUri` is returned.
fn find_widevine_key(manifest: &str, ctx: &ManifestContext) -> Result<(Vec<u8>, String), DrmError> {
    if let Some(found) = scan_key_lines(manifest, "#EXT-X-KEY:")? {
        return Ok(found);
    }
    if let Some(found) = scan_key_lines(manifest, "#EXT-X-SESSION-KEY:")? {
        tracing::debug!("selected Widevine key from #EXT-X-SESSION-KEY");
        return Ok(found);
    }
    dump_manifest_diagnostic(ctx, "MediaPlaylist", manifest, None);
    Err(DrmError::MissingKeyUri)
}

/// Scans every line with `prefix` for a Widevine key entry (see
/// [`find_widevine_key`] for the match criteria). Returns `Ok(None)` if no
/// line qualifies. Invalid base64 in the `data:` URI surfaces as
/// [`DrmError::HlsParse`].
fn scan_key_lines(manifest: &str, prefix: &str) -> Result<Option<(Vec<u8>, String)>, DrmError> {
    let mut best: Option<(KeyMatchRank, Vec<u8>, String)> = None;
    for line in manifest.lines() {
        let Some(attrs) = line.strip_prefix(prefix) else {
            continue;
        };
        let parsed = parse_attr_list(attrs);
        let Some(rank) = classify_key_line(&parsed) else {
            continue;
        };
        let Some(uri) = parsed.get("URI").cloned() else {
            continue;
        };
        // Accept both `data:;base64,…` and `data:<type>;base64,…`.
        let Some((_, b64)) = uri.split_once("base64,") else {
            continue;
        };
        let decoded = BASE64_STANDARD
            .decode(b64)
            .map_err(|e| DrmError::HlsParse(format!("bad key data: {e}")))?;
        let pssh = ensure_pssh_bytes(&decoded);

        // Higher rank wins; equal rank → keep the first-seen match.
        if best.as_ref().is_none_or(|(prev, _, _)| rank > *prev) {
            best = Some((rank, pssh, uri));
        }
    }
    Ok(best.map(|(_, pssh, uri)| (pssh, uri)))
}

/// Classifies an `#EXT-X-KEY` / `#EXT-X-SESSION-KEY` attribute set as a
/// Widevine candidate. Returns `None` when neither signal is present.
fn classify_key_line(attrs: &HashMap<String, String>) -> Option<KeyMatchRank> {
    if attrs
        .get("KEYFORMAT")
        .is_some_and(|kf| kf.eq_ignore_ascii_case(WIDEVINE_KEYFORMAT_UUID))
    {
        return Some(KeyMatchRank::WidevineKeyformat);
    }
    match attrs.get("METHOD") {
        Some(m) if m.eq_ignore_ascii_case("ISO-23001-7") => Some(KeyMatchRank::Iso23001_7),
        Some(m) if m.eq_ignore_ascii_case("SAMPLE-AES-CTR") => Some(KeyMatchRank::SampleAesCtr),
        _ => None,
    }
}

/// Returns a well-formed Widevine PSSH box for the given decoded key data.
///
/// - If `decoded` already begins with the ISO-BMFF `pssh` box type signature
///   (`data[4..8] == b"pssh"`), it is returned unchanged.
/// - Otherwise — Apple's real AAC CENC manifests ship a bare 16-byte
///   `default_KID` in the `data:;base64,…` payload — the bytes are wrapped
///   into a Widevine v0 PSSH box via [`crate::cdm::build_pssh_box`].
fn ensure_pssh_bytes(decoded: &[u8]) -> Vec<u8> {
    if is_pssh_box(decoded) {
        return decoded.to_vec();
    }
    crate::cdm::build_pssh_box(decoded)
}

/// Detects an ISO-BMFF `pssh` box by its 4-byte box-type signature at
/// offset 4. Does not validate the system id or the box length.
fn is_pssh_box(data: &[u8]) -> bool {
    data.len() >= 8 && &data[4..8] == b"pssh"
}

/// Emits a `tracing::warn!` event with everything an operator would want to
/// see when Widevine key selection fails on a live manifest. Includes the
/// chosen asset URL + flavor, the HTTP status + content-type of the fetch,
/// the parser's classification, every `#EXT-X-KEY` / `#EXT-X-SESSION-KEY`
/// line verbatim, line counts, and the manifest body (full when ≤4 KiB,
/// otherwise a ~2 KiB prefix).
///
/// Never logs request headers or tokens — the manifest body is public CDN
/// content and carries no auth material.
///
/// Suppressed when `ctx.asset_url` is empty so unit-test calls don't produce
/// noise.
fn dump_manifest_diagnostic(
    ctx: &ManifestContext,
    classification: &str,
    body: &str,
    parse_error: Option<&str>,
) {
    const FULL_THRESHOLD: usize = 4096;
    const PREVIEW_BYTES: usize = 2048;

    if ctx.asset_url.is_empty() {
        return;
    }

    let total_lines = body.lines().count();
    let key_lines: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("#EXT-X-KEY:"))
        .collect();
    let session_key_lines: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("#EXT-X-SESSION-KEY:"))
        .collect();

    let body_preview = if body.len() <= FULL_THRESHOLD {
        body.to_owned()
    } else {
        let mut end = PREVIEW_BYTES.min(body.len());
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n…[truncated; {} bytes total]", &body[..end], body.len())
    };

    // Strip the query string before logging: Apple's music-video `mvod`
    // URLs carry `dsid` (account id) and `accessKey` as query parameters
    // and WARN-level logs are not the place for account identifiers.
    let scrubbed_url = scrub_url_query(&ctx.asset_url);

    tracing::warn!(
        asset_url = %scrubbed_url,
        flavor = %ctx.flavor,
        http_status = ctx.http_status,
        content_type = %ctx.content_type,
        classification = %classification,
        total_lines,
        ext_x_key_count = key_lines.len(),
        ext_x_session_key_count = session_key_lines.len(),
        key_lines = ?key_lines,
        session_key_lines = ?session_key_lines,
        parse_error = ?parse_error,
        body_preview = %body_preview,
        "HLS manifest diagnostic dump (Widevine key selection failed)"
    );
}

/// Returns a logging-safe version of `url`: host + path (and port, if any)
/// with the query string and fragment stripped. Falls back to the literal
/// placeholder `"<unparseable-url>"` when the input is not a valid URL so
/// we never fall through to logging raw account identifiers on malformed
/// input.
fn scrub_url_query(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<unparseable-url>".to_owned(),
    }
}

/// Parses an `#EXT-X-KEY` attribute list (`A=B,C="D,E"`) honouring quoted
/// values. Values are returned with surrounding quotes stripped.
fn parse_attr_list(input: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // key up to '='
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = input[key_start..i].trim().to_owned();
        i += 1; // skip '='

        // value up to top-level ','
        let mut quoted = false;
        let val_start = i;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'"' {
                quoted = !quoted;
            } else if c == b',' && !quoted {
                break;
            }
            i += 1;
        }
        let mut val = &input[val_start..i];
        if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
            val = &val[1..val.len() - 1];
        }
        out.insert(key, val.to_owned());
        if i < bytes.len() {
            i += 1; // skip ','
        }
    }
    out
}

/// Resolves `relative` against `base` using URL semantics.
pub fn resolve_url(base: &str, relative: &str) -> Result<String, DrmError> {
    if relative.starts_with("http://") || relative.starts_with("https://") {
        return Ok(relative.to_owned());
    }
    let base_url = url::Url::parse(base).map_err(|e| DrmError::InvalidUri(e.to_string()))?;
    let joined = base_url
        .join(relative)
        .map_err(|e| DrmError::InvalidUri(e.to_string()))?;
    Ok(joined.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MEDIA_M3U8: &str = "#EXTM3U\n\
#EXT-X-VERSION:6\n\
#EXT-X-TARGETDURATION:6\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://p1/key1\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n\
#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:;base64,AAAAAAAAAAAAAAAAAAAAAA==\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
#EXT-X-MAP:URI=\"segment0.mp4\"\n\
#EXTINF:5.986,\n\
segment1.mp4\n\
#EXTINF:5.986,\n\
segment2.mp4\n\
#EXT-X-ENDLIST\n";

    /// Test helper: extracts the first `key_id` carried in a Widevine PSSH
    /// box by locating the length-delimited field-2 marker in the payload.
    /// Parses the on-the-wire box directly instead of calling back into the
    /// CDM so the test asserts against the actual bytes handed to the CDM.
    fn kid_from_wrapped_pssh(pssh: &[u8]) -> Vec<u8> {
        assert!(
            is_pssh_box(pssh),
            "ensure_pssh_bytes output must be a PSSH box"
        );
        // Layout: 4 size + 4 "pssh" + 4 (ver+flags) + 16 sys_id + 4 data_size + data.
        let data_size = u32::from_be_bytes(pssh[28..32].try_into().unwrap()) as usize;
        let payload = &pssh[32..32 + data_size];
        // WidevineCencHeader fields (in any order prost emits):
        //   tag=1 (algorithm, varint)         → 0x08 <varint>
        //   tag=2 (key_id, length-delimited)  → 0x12 <len:varint> <bytes>
        // Scan for the first 0x12 marker and return that many bytes.
        let idx = payload
            .iter()
            .position(|&b| b == 0x12)
            .expect("WidevineCencHeader payload has no key_id field");
        let len = usize::from(payload[idx + 1]);
        payload[idx + 2..idx + 2 + len].to_vec()
    }

    #[test]
    fn should_parse_widevine_key_from_multi_key_manifest() {
        let info = parse_media_playlist(
            SAMPLE_MEDIA_M3U8,
            "https://aod-ssl.itunes.apple.com/p1/master.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        let kid = kid_from_wrapped_pssh(&info.pssh_init_data);
        assert_eq!(kid.len(), 16);
        assert!(kid.iter().all(|&b| b == 0));
        assert!(info.key_uri.starts_with("data:;base64,"));
    }

    #[test]
    fn should_resolve_segment_and_init_urls() {
        let info = parse_media_playlist(
            SAMPLE_MEDIA_M3U8,
            "https://aod-ssl.itunes.apple.com/p1/master.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(
            info.init_segment.url,
            "https://aod-ssl.itunes.apple.com/p1/segment0.mp4"
        );
        assert!(info.init_segment.byte_range.is_none());
        let seg_urls: Vec<&str> = info.segments.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            seg_urls,
            vec![
                "https://aod-ssl.itunes.apple.com/p1/segment1.mp4",
                "https://aod-ssl.itunes.apple.com/p1/segment2.mp4",
            ]
        );
        assert!(info.segments.iter().all(|s| s.byte_range.is_none()));
    }

    #[test]
    fn missing_widevine_key_should_fail() {
        let manifest = "#EXTM3U\n\
#EXT-X-TARGETDURATION:6\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://p1/key1\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
#EXT-X-MAP:URI=\"segment0.mp4\"\n\
#EXTINF:5.986,\n\
segment1.mp4\n\
#EXT-X-ENDLIST\n";
        let err = parse_media_playlist(
            manifest,
            "https://cdn.example.com/x.m3u8",
            &ManifestContext::default(),
        )
        .unwrap_err();
        assert!(matches!(err, DrmError::MissingKeyUri));
    }

    #[test]
    fn missing_segments_should_fail() {
        let manifest = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n";
        let err = parse_media_playlist(
            manifest,
            "https://cdn.example.com/x.m3u8",
            &ManifestContext::default(),
        )
        .unwrap_err();
        assert!(matches!(err, DrmError::MissingSegments));
    }

    #[test]
    fn resolve_absolute_url_should_pass_through() {
        let v = resolve_url("https://a.com/x", "https://b.com/y").unwrap();
        assert_eq!(v, "https://b.com/y");
    }

    #[test]
    fn resolve_relative_should_replace_last_segment() {
        let v = resolve_url("https://a.com/x/y.m3u8", "z.mp4").unwrap();
        assert_eq!(v, "https://a.com/x/z.mp4");
    }

    #[test]
    fn pick_widevine_asset_prefers_256() {
        let assets = vec![
            Asset {
                flavor: "32:ctrp64".into(),
                url: "u64".into(),
            },
            Asset {
                flavor: "28:ctrp256".into(),
                url: "u256".into(),
            },
            Asset {
                flavor: "30:cbcp256".into(),
                url: "cbc".into(),
            },
        ];
        let chosen = pick_widevine_asset(&assets).unwrap();
        assert_eq!(chosen.url, "u256");
    }

    #[test]
    fn pick_widevine_asset_falls_back_to_64() {
        let assets = vec![
            Asset {
                flavor: "32:ctrp64".into(),
                url: "u64".into(),
            },
            Asset {
                flavor: "30:cbcp256".into(),
                url: "cbc".into(),
            },
        ];
        let chosen = pick_widevine_asset(&assets).unwrap();
        assert_eq!(chosen.url, "u64");
    }

    #[test]
    fn pick_widevine_asset_none_when_only_fairplay() {
        let assets = vec![Asset {
            flavor: "30:cbcp256".into(),
            url: "cbc".into(),
        }];
        assert!(pick_widevine_asset(&assets).is_none());
    }

    #[test]
    fn parse_attr_list_handles_quoted_commas() {
        let m = parse_attr_list(r#"A="x,y",B=1,C="q""#);
        assert_eq!(m.get("A").unwrap(), "x,y");
        assert_eq!(m.get("B").unwrap(), "1");
        assert_eq!(m.get("C").unwrap(), "q");
    }

    /// Mirrors a real Apple Music media playlist: `FairPlay` `#EXT-X-KEY` listed
    /// first, Widevine second. The selector must skip the `FairPlay` line, decode
    /// the Widevine `data:…;base64,…` payload byte-for-byte, and preserve the
    /// full `data:` URI verbatim so it can be echoed into the license-request
    /// body.
    #[test]
    fn should_pick_widevine_over_fairplay_with_roundtrip_pssh() {
        let expected: &[u8] = b"0123456789abcdef";
        let b64 = BASE64_STANDARD.encode(expected);
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://p1/key1\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:text/plain;base64,{b64}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-MAP:URI=\"segment0.mp4\"\n\
             #EXTINF:5.986,\n\
             segment1.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        // Widevine entry, not FairPlay's `skd://…` URI.
        assert_eq!(kid_from_wrapped_pssh(&info.pssh_init_data), expected);
        let expected_uri = format!("data:text/plain;base64,{b64}");
        assert_eq!(
            info.key_uri, expected_uri,
            "the verbatim data: URI must be preserved for the license-request body"
        );
        assert!(!info.key_uri.starts_with("skd://"));
    }

    /// `#EXT-X-KEY` attribute order is free per RFC 8216 §4.2 — `KEYFORMAT` may
    /// precede `URI`. Parsing must not depend on positional order, and must
    /// still accept an `application/octet-stream` data-URI media type.
    #[test]
    fn should_parse_widevine_key_with_reverse_attr_order() {
        let expected: &[u8] = &[
            0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77,
        ];
        let b64 = BASE64_STANDARD.encode(expected);
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-KEY:KEYFORMAT=\"com.apple.streamingkeydelivery\",METHOD=SAMPLE-AES,KEYFORMATVERSIONS=\"1\",URI=\"skd://p1/key1\"\n\
             #EXT-X-KEY:KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\",METHOD=SAMPLE-AES-CTR,URI=\"data:application/octet-stream;base64,{b64}\"\n\
             #EXT-X-MAP:URI=\"segment0.mp4\"\n\
             #EXTINF:5.986,\n\
             segment1.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(kid_from_wrapped_pssh(&info.pssh_init_data), expected);
        let expected_uri = format!("data:application/octet-stream;base64,{b64}");
        assert_eq!(info.key_uri, expected_uri);
    }

    /// The selector must accept `METHOD=SAMPLE-AES-CTR` (Apple CENC's real
    /// value) and not silently filter on a plain `SAMPLE-AES` METHOD.
    #[test]
    fn should_accept_sample_aes_ctr_method() {
        // `b"defg"` → base64 `ZGVmZw==`.
        let manifest = "#EXTM3U\n\
#EXT-X-VERSION:6\n\
#EXT-X-TARGETDURATION:6\n\
#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:;base64,ZGVmZw==\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
#EXT-X-MAP:URI=\"segment0.mp4\"\n\
#EXTINF:5.986,\n\
segment1.mp4\n\
#EXT-X-ENDLIST\n";
        let info = parse_media_playlist(
            manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        // b"defg" is 4 bytes (<16) so it isn't a valid KID and ensure_pssh_bytes
        // still wraps it; this test's focus is METHOD=SAMPLE-AES-CTR + Widevine
        // KEYFORMAT selection, not PSSH-box roundtripping.
        assert!(is_pssh_box(&info.pssh_init_data));
        assert_eq!(info.key_uri, "data:;base64,ZGVmZw==");
    }

    /// When the manifest carries ONLY `#EXT-X-SESSION-KEY` (no segment-level
    /// `#EXT-X-KEY`), the selector falls back to the session-level Widevine
    /// key and preserves its `data:` URI verbatim for the license-request
    /// body.
    #[test]
    fn should_fall_back_to_session_key_when_no_segment_key() {
        let expected: &[u8] = b"sessionkey_16byt";
        let b64 = BASE64_STANDARD.encode(expected);
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-SESSION-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:;base64,{b64}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-MAP:URI=\"segment0.mp4\"\n\
             #EXTINF:5.986,\n\
             segment1.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(kid_from_wrapped_pssh(&info.pssh_init_data), expected);
        let expected_uri = format!("data:;base64,{b64}");
        assert_eq!(
            info.key_uri, expected_uri,
            "the verbatim data: URI must be preserved for the license-request body"
        );
    }

    /// When both `#EXT-X-KEY` and `#EXT-X-SESSION-KEY` carry Widevine entries,
    /// the segment-level key wins. Per RFC 8216 §4.3.4.5 a segment-level
    /// `#EXT-X-KEY` overrides any earlier session-level `#EXT-X-SESSION-KEY`
    /// for the segments that follow, so segment-level is the authoritative
    /// value when both are present.
    #[test]
    fn should_prefer_segment_key_over_session_key_when_both_present() {
        let segment_key: &[u8] = b"segmentkey_16byt";
        let session_level: &[u8] = b"sessionkey_16byt";
        let segment_b64 = BASE64_STANDARD.encode(segment_key);
        let session_b64 = BASE64_STANDARD.encode(session_level);
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-SESSION-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:;base64,{session_b64}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:;base64,{segment_b64}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-MAP:URI=\"segment0.mp4\"\n\
             #EXTINF:5.986,\n\
             segment1.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(
            kid_from_wrapped_pssh(&info.pssh_init_data),
            segment_key,
            "segment-level #EXT-X-KEY must take precedence over #EXT-X-SESSION-KEY"
        );
        let expected_uri = format!("data:;base64,{segment_b64}");
        assert_eq!(info.key_uri, expected_uri);
    }

    // -------------------------------------------------------------------
    // Apple's real AAC CENC manifest shape.
    // -------------------------------------------------------------------

    /// Apple's live `.m3u8` emits the Widevine key with `METHOD=ISO-23001-7`
    /// and NO `KEYFORMAT` attribute. The selector must pick this line
    /// even without a Widevine `KEYFORMAT`, preserve the exact
    /// `data:;base64,…` URI verbatim for the license-request body, and
    /// deliver a Widevine PSSH box whose KID is the 16 decoded bytes.
    #[test]
    fn should_select_key_by_method_iso_23001_7_without_keyformat() {
        // The real captured line, byte-for-byte:
        let manifest = "#EXTM3U\n\
#EXT-X-VERSION:6\n\
#EXT-X-TARGETDURATION:6\n\
#EXT-X-KEY:METHOD=ISO-23001-7,URI=\"data:;base64,AAAAADkxyM0AHX+PF5AB7Q==\"\n\
#EXT-X-MAP:URI=\"segment0.mp4\"\n\
#EXTINF:5.986,\n\
segment1.mp4\n\
#EXT-X-ENDLIST\n";
        let info = parse_media_playlist(
            manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        // Verbatim URI preserved for the acquireWebPlaybackLicense body.
        assert_eq!(info.key_uri, "data:;base64,AAAAADkxyM0AHX+PF5AB7Q==");
        // The payload is a bare 16-byte KID; the selector wraps it.
        let expected_kid = BASE64_STANDARD.decode("AAAAADkxyM0AHX+PF5AB7Q==").unwrap();
        assert_eq!(expected_kid.len(), 16);
        assert_eq!(kid_from_wrapped_pssh(&info.pssh_init_data), expected_kid);
    }

    /// `ensure_pssh_bytes` must wrap a bare 16-byte KID into a Widevine v0
    /// PSSH box whose `pssh` box type, Widevine system id, and protobuf
    /// `key_ids[0]` all round-trip.
    #[test]
    fn should_wrap_bare_kid_into_pssh_box() {
        let kid = *b"0123456789abcdef";
        let out = ensure_pssh_bytes(&kid);
        // `pssh` box type signature at offset 4.
        assert_eq!(&out[4..8], b"pssh");
        // Widevine system id at offset 12.
        assert_eq!(
            &out[12..28],
            [
                0xED, 0xEF, 0x8B, 0xA9, 0x79, 0xD6, 0x4A, 0xCE, 0xA3, 0xC8, 0x27, 0xDC, 0xD5, 0x1D,
                0x21, 0xED
            ]
        );
        // Protobuf payload decodes back to the KID as `WidevineCencHeader.key_id[0]`.
        assert_eq!(kid_from_wrapped_pssh(&out), kid.to_vec());
    }

    /// `ensure_pssh_bytes` must pass a pre-wrapped PSSH box through unchanged,
    /// so services that ship a full PSSH in the data URI are not re-wrapped.
    #[test]
    fn should_pass_through_existing_pssh_box() {
        // Not a real PSSH box, just something that trips the `pssh` signature
        // detection at offset 4. The selector must not re-wrap or truncate.
        let existing: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(&[0x00, 0x00, 0x00, 0x20]); // size = 32 (nominal)
            v.extend_from_slice(b"pssh");
            v.extend_from_slice(&[0, 0, 0, 0]); // version + flags
            v.extend_from_slice(&[
                0xED, 0xEF, 0x8B, 0xA9, 0x79, 0xD6, 0x4A, 0xCE, 0xA3, 0xC8, 0x27, 0xDC, 0xD5, 0x1D,
                0x21, 0xED,
            ]);
            v.extend_from_slice(&[0, 0, 0, 0]); // data_size = 0
            v
        };
        let out = ensure_pssh_bytes(&existing);
        assert_eq!(
            out, existing,
            "already-PSSH bytes must pass through unchanged"
        );
    }

    /// Safety-net path: when a manifest DOES ship `KEYFORMAT="urn:uuid:…"`
    /// (Apple's older variants, or other services), the selector still
    /// recognizes it even without an ISO-23001-7 / SAMPLE-AES-CTR METHOD.
    #[test]
    fn should_still_select_by_keyformat_when_present() {
        let kid: &[u8] = &[
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ];
        let b64 = BASE64_STANDARD.encode(kid);
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-KEY:METHOD=SOMETHING-WEIRD,URI=\"data:;base64,{b64}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-MAP:URI=\"segment0.mp4\"\n\
             #EXTINF:5.986,\n\
             segment1.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(kid_from_wrapped_pssh(&info.pssh_init_data), kid);
        let expected_uri = format!("data:;base64,{b64}");
        assert_eq!(info.key_uri, expected_uri);
    }

    /// Priority check: when several candidates coexist, KEYFORMAT-Widevine
    /// beats ISO-23001-7 which beats SAMPLE-AES-CTR. First-in-file breaks
    /// ties within the same rank.
    #[test]
    fn should_prefer_widevine_keyformat_over_method_based_matches() {
        let sample_ctr_kid: &[u8] = &[0x11; 16];
        let iso_kid: &[u8] = &[0x22; 16];
        let keyformat_kid: &[u8] = &[0x33; 16];
        let sample_ctr_b64 = BASE64_STANDARD.encode(sample_ctr_kid);
        let iso_b64 = BASE64_STANDARD.encode(iso_kid);
        let keyformat_b64 = BASE64_STANDARD.encode(keyformat_kid);
        // Intentionally list them in the WEAKEST-first order so the test
        // proves selection is by rank, not by file order.
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:;base64,{sample_ctr_b64}\"\n\
             #EXT-X-KEY:METHOD=ISO-23001-7,URI=\"data:;base64,{iso_b64}\"\n\
             #EXT-X-KEY:METHOD=UNKNOWN,URI=\"data:;base64,{keyformat_b64}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n\
             #EXT-X-MAP:URI=\"segment0.mp4\"\n\
             #EXTINF:5.986,\n\
             segment1.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://aod-ssl.itunes.apple.com/p1/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(
            kid_from_wrapped_pssh(&info.pssh_init_data),
            keyformat_kid,
            "KEYFORMAT=Widevine must outrank METHOD-based matches"
        );
    }

    // -------------------------------------------------------------------
    // #EXT-X-BYTERANGE / #EXT-X-MAP BYTERANGE handling (RFC 8216 §4.3.2.2).
    // -------------------------------------------------------------------

    /// Minimal Widevine `#EXT-X-KEY` line used across the byterange tests.
    const WIDEVINE_KEY_LINE: &str =
        "#EXT-X-KEY:METHOD=ISO-23001-7,URI=\"data:;base64,AAAAADkxyM0AHX+PF5AB7Q==\"";

    /// A playlist with explicit `@offset` on every segment must round-trip the
    /// exact `(offset, length)` pair onto each `SegmentRange`.
    #[test]
    fn parse_segments_with_explicit_byte_range_offsets() {
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             {WIDEVINE_KEY_LINE}\n\
             #EXT-X-MAP:URI=\"a.mp4\"\n\
             #EXT-X-BYTERANGE:1000@10\n\
             a.mp4\n\
             #EXT-X-BYTERANGE:2000@1010\n\
             a.mp4\n\
             #EXT-X-BYTERANGE:500@3010\n\
             a.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://cdn.example.com/x.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        let ranges: Vec<Option<(u64, u64)>> = info.segments.iter().map(|s| s.byte_range).collect();
        assert_eq!(
            ranges,
            vec![Some((10, 1000)), Some((1010, 2000)), Some((3010, 500))]
        );
    }

    /// Implicit `@offset` (`#EXT-X-BYTERANGE:<n>`) must resolve to the end of
    /// the previous same-URL range per RFC 8216 §4.3.2.2.
    #[test]
    fn parse_segments_with_implicit_byte_range_offsets() {
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             {WIDEVINE_KEY_LINE}\n\
             #EXT-X-MAP:URI=\"a.mp4\"\n\
             #EXT-X-BYTERANGE:100@0\n\
             a.mp4\n\
             #EXT-X-BYTERANGE:200\n\
             a.mp4\n\
             #EXT-X-BYTERANGE:50\n\
             a.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://cdn.example.com/x.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        let ranges: Vec<Option<(u64, u64)>> = info.segments.iter().map(|s| s.byte_range).collect();
        assert_eq!(
            ranges,
            vec![Some((0, 100)), Some((100, 200)), Some((300, 50))]
        );
    }

    /// When a segment switches to a different URL, the implicit-offset running
    /// counter must reset per URL.
    #[test]
    fn parse_segments_reset_implicit_offset_on_new_url() {
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             {WIDEVINE_KEY_LINE}\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXT-X-BYTERANGE:100@0\n\
             a.mp4\n\
             #EXT-X-BYTERANGE:200@0\n\
             b.mp4\n\
             #EXT-X-BYTERANGE:50\n\
             a.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://cdn.example.com/x.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        let urls: Vec<&str> = info.segments.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://cdn.example.com/a.mp4",
                "https://cdn.example.com/b.mp4",
                "https://cdn.example.com/a.mp4",
            ]
        );
        let ranges: Vec<Option<(u64, u64)>> = info.segments.iter().map(|s| s.byte_range).collect();
        // Third segment's implicit offset must resume from a.mp4's running
        // end (100) — NOT from b.mp4's end (200).
        assert_eq!(
            ranges,
            vec![Some((0, 100)), Some((0, 200)), Some((100, 50))]
        );
    }

    /// `#EXT-X-MAP:BYTERANGE="N@O"` must land on `HlsInfo.init_segment.byte_range`
    /// and seed the running offset for its URL so the first segment's implicit
    /// offset (if any) starts from the end of the init range.
    #[test]
    fn parse_init_map_byte_range() {
        let manifest = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:6\n\
             #EXT-X-TARGETDURATION:6\n\
             {WIDEVINE_KEY_LINE}\n\
             #EXT-X-MAP:URI=\"a.mp4\",BYTERANGE=\"1006@0\"\n\
             #EXT-X-BYTERANGE:390817\n\
             a.mp4\n\
             #EXT-X-ENDLIST\n"
        );
        let info = parse_media_playlist(
            &manifest,
            "https://cdn.example.com/x.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();
        assert_eq!(info.init_segment.url, "https://cdn.example.com/a.mp4");
        assert_eq!(info.init_segment.byte_range, Some((0, 1006)));
        // The first segment's implicit offset must continue from the init's
        // end on the same URL: 0 + 1006 = 1006.
        assert_eq!(info.segments.len(), 1);
        assert_eq!(info.segments[0].byte_range, Some((1006, 390_817)));
    }

    // -------------------------------------------------------------------
    // Regression fixtures captured from real public-catalog responses.
    // -------------------------------------------------------------------

    /// Regression: the exact `#EXT-X-KEY` line captured from the Pt. 2 song
    /// (`salableAdamId=1443063973`): `METHOD=ISO-23001-7`, NO
    /// `KEYFORMAT`, `URI="data:;base64,<16-byte KID>"`. The selector must
    /// pick it, round-trip the URI byte-for-byte for the subsequent
    /// `acquireWebPlaybackLicense` body, and wrap the bare 16-byte KID into a
    /// Widevine v0 PSSH box via `ensure_pssh_bytes`.
    #[test]
    fn pt2_song_ext_x_key_line_wraps_bare_kid_into_pssh() {
        // Verbatim capture: `data:;base64,AAAAAFYDaKUAHUGjIU7ACQ==`.
        const KEY_URI: &str = "data:;base64,AAAAAFYDaKUAHUGjIU7ACQ==";
        // The KID inside it: `00000000560368a5001d41a3214ec009`.
        const EXPECTED_KID: [u8; 16] = [
            0x00, 0x00, 0x00, 0x00, 0x56, 0x03, 0x68, 0xa5, 0x00, 0x1d, 0x41, 0xa3, 0x21, 0x4e,
            0xc0, 0x09,
        ];
        let manifest = "#EXTM3U\n\
#EXT-X-TARGETDURATION:10\n\
#EXT-X-VERSION:7\n\
#EXT-X-MEDIA-SEQUENCE:0\n\
#EXT-X-PLAYLIST-TYPE:VOD\n\
#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-KEY:METHOD=ISO-23001-7,URI=\"data:;base64,AAAAAFYDaKUAHUGjIU7ACQ==\"\n\
#EXT-X-MAP:URI=\"mzaf_A1443063973.rmhq.aac.wa.mp4\",BYTERANGE=\"1006@0\"\n\
#EXTINF:9.98458,\n\
#EXT-X-BYTERANGE:347666@1006\n\
mzaf_A1443063973.rmhq.aac.wa.mp4\n\
#EXT-X-ENDLIST\n";

        let info = parse_media_playlist(
            manifest,
            "https://aod-ssl.itunes.apple.com/p1/mzaf_A1443063973.rphq.aac.wa.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();

        // (1) The license-request `uri` is sent verbatim; never re-encoded.
        assert_eq!(info.key_uri, KEY_URI);
        // (2) The raw payload is a bare 16-byte KID.
        let decoded = BASE64_STANDARD.decode("AAAAAFYDaKUAHUGjIU7ACQ==").unwrap();
        assert_eq!(decoded.len(), 16);
        assert_eq!(decoded.as_slice(), EXPECTED_KID.as_slice());
        // (3) `ensure_pssh_bytes` wraps it into a Widevine v0 PSSH box —
        //     `pssh` signature at offset 4 and Widevine system id at offset 12.
        assert_eq!(&info.pssh_init_data[4..8], b"pssh");
        assert_eq!(
            &info.pssh_init_data[12..28],
            [
                0xED, 0xEF, 0x8B, 0xA9, 0x79, 0xD6, 0x4A, 0xCE, 0xA3, 0xC8, 0x27, 0xDC, 0xD5, 0x1D,
                0x21, 0xED,
            ]
        );
        assert_eq!(kid_from_wrapped_pssh(&info.pssh_init_data), EXPECTED_KID);
    }

    /// Regression: the exact three-`#EXT-X-KEY` block captured from the KING
    /// music-video audio playlist. Lines are `FairPlay` (`skd://`,
    /// `KEYFORMAT="com.apple.streamingkeydelivery"`), `PlayReady`
    /// (`KEYFORMAT="com.microsoft.playready"`), and Widevine
    /// (`KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"`), in that
    /// order. The selector must skip the first two and pick the Widevine one;
    /// its `URI` already carries a full `pssh` box, so `ensure_pssh_bytes`
    /// must pass the decoded bytes through unchanged.
    #[test]
    fn king_music_video_ext_x_key_block_prefers_widevine_keyformat_line() {
        // Verbatim capture.
        const WIDEVINE_URI: &str = "data:text/plain;base64,AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAABUZUSuYzYgICAgICBI88aJmwY=";
        const WIDEVINE_B64: &str =
            "AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAABUZUSuYzYgICAgICBI88aJmwY=";
        // The PlayReady line's URI is truncated to a short valid base64; the
        // parser skips the PlayReady line at `classify_key_line` (KEYFORMAT is
        // `com.microsoft.playready`, METHOD is `SAMPLE-AES` — neither matches)
        // before attempting any base64 decode, so this truncation is safe.
        let manifest = format!(
            "#EXTM3U\n\
#EXT-X-TARGETDURATION:6\n\
#EXT-X-VERSION:7\n\
#EXT-X-MEDIA-SEQUENCE:0\n\
#EXT-X-PLAYLIST-TYPE:VOD\n\
#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://itunes.apple.com/p1415922862/c6\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"data:text/plain;charset=UTF-16;base64,vgEA\",KEYFORMAT=\"com.microsoft.playready\",KEYFORMATVERSIONS=\"1\"\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"{WIDEVINE_URI}\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
#EXT-X-MAP:URI=\"P1415922862_A6782267818_audio_en_gr256_mp4a-40-2-0.mp4\"\n\
#EXTINF:5.99467,\n\
P1415922862_A6782267818_audio_en_gr256_mp4a-40-2-1.m4s\n\
#EXT-X-ENDLIST\n"
        );

        let info = parse_media_playlist(
            &manifest,
            "https://mvod.example.com/p/a.m3u8",
            &ManifestContext::default(),
        )
        .unwrap();

        // (1) The Widevine line wins over the FairPlay (`skd://`) and
        //     PlayReady lines even though they come first in the block.
        assert_eq!(info.key_uri, WIDEVINE_URI);
        // (2) The decoded payload is already a full `pssh` box (bytes 4..8 ==
        //     "pssh") and `ensure_pssh_bytes` returns it byte-identical.
        let decoded = BASE64_STANDARD.decode(WIDEVINE_B64).unwrap();
        assert_eq!(&decoded[4..8], b"pssh");
        assert_eq!(info.pssh_init_data, decoded);
    }

    // -------------------------------------------------------------------
    // Music video: master-playlist audio-rendition picker.
    // -------------------------------------------------------------------

    /// Regression: the KING master playlist's `#EXT-X-MEDIA` block
    /// carries audio renditions `audio-HE2-stereo-32` / `audio-stereo-64` /
    /// `audio-stereo-128` / `audio-stereo-256` plus several video variants.
    /// `pick_audio` must return the `audio-stereo-256` URI — never a video
    /// variant and never the HE-AAC v2 rendition.
    #[test]
    fn pick_audio_picks_stereo_256_from_king_master_playlist() {
        let master_text = "#EXTM3U\n\
#EXT-X-VERSION:7\n\
#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"en\",GROUP-ID=\"audio-HE2-stereo-32\",NAME=\"English\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"https://mvod.example.com/P1_audio_en_gr32_mp4a-40-2.m3u8?accessKey=aaa\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"en\",GROUP-ID=\"audio-stereo-64\",NAME=\"English\",DEFAULT=NO,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"https://mvod.example.com/P1_audio_en_gr64_mp4a-40-2.m3u8?accessKey=bbb\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"en\",GROUP-ID=\"audio-stereo-128\",NAME=\"English\",DEFAULT=NO,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"https://mvod.example.com/P1_audio_en_gr128_mp4a-40-2.m3u8?accessKey=ccc\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"en\",GROUP-ID=\"audio-stereo-256\",NAME=\"English\",DEFAULT=NO,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"https://mvod.example.com/P1415922862_A6782267818_audio_en_gr256_mp4a-40-2.m3u8?accessKey=ddd\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"audio-stereo-256\",RESOLUTION=1920x1080\n\
https://mvod.example.com/P1_v1080p.m3u8?accessKey=video\n\
#EXT-X-STREAM-INF:BANDWIDTH=1500000,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"audio-stereo-256\",RESOLUTION=1280x720\n\
https://mvod.example.com/P1_v720p.m3u8?accessKey=video\n";

        let master = m3u8_rs::parse_master_playlist_res(master_text.as_bytes())
            .expect("master playlist parses");
        let picked = pick_audio(&master).expect("audio-stereo-256 rendition present");
        assert_eq!(
            picked,
            "https://mvod.example.com/P1415922862_A6782267818_audio_en_gr256_mp4a-40-2.m3u8?accessKey=ddd",
            "pick_audio must return the stereo-256 (AAC-LC) audio rendition, never a video variant"
        );
    }

    /// When no `audio-stereo-*` rendition is present (only the HE-AAC v2
    /// rendition or none at all), `pick_audio` must return `None` so the
    /// caller fails fast with `NoAudioAsset` instead of accidentally picking a
    /// video rendition.
    #[test]
    fn pick_audio_returns_none_when_only_he_aac_v2_is_present() {
        let master_text = "#EXTM3U\n\
#EXT-X-VERSION:7\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-HE2-stereo-32\",NAME=\"English\",URI=\"https://mvod.example.com/P1_audio_en_gr32_mp4a-40-2.m3u8?accessKey=aaa\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"audio-HE2-stereo-32\",RESOLUTION=1920x1080\n\
https://mvod.example.com/P1_v1080p.m3u8?accessKey=video\n";

        let master = m3u8_rs::parse_master_playlist_res(master_text.as_bytes()).unwrap();
        assert!(pick_audio(&master).is_none());
    }

    /// `pick_audio` falls through 256 → 128 → 64 when higher-quality
    /// renditions are missing.
    #[test]
    fn pick_audio_falls_back_to_lower_bitrate_audio_rendition() {
        let master_text = "#EXTM3U\n\
#EXT-X-VERSION:7\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-stereo-64\",NAME=\"English\",URI=\"https://mvod.example.com/gr64.m3u8?accessKey=k\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-stereo-128\",NAME=\"English\",URI=\"https://mvod.example.com/gr128.m3u8?accessKey=k\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1500000,CODECS=\"avc1.4d401f\",AUDIO=\"audio-stereo-128\",RESOLUTION=1280x720\n\
https://mvod.example.com/v720p.m3u8?accessKey=video\n";
        let master = m3u8_rs::parse_master_playlist_res(master_text.as_bytes()).unwrap();
        assert_eq!(
            pick_audio(&master).unwrap(),
            "https://mvod.example.com/gr128.m3u8?accessKey=k"
        );
    }

    /// `scrub_url_query` must strip every query parameter (even those carrying
    /// `dsid` / `accessKey`) and the fragment, keeping host + path only, so
    /// WARN-level manifest dumps never leak account identifiers.
    #[test]
    fn scrub_url_query_removes_dsid_and_access_key() {
        let raw = "https://mvod.example.com/P1/audio.m3u8?dsid=1234567890&accessKey=secret&id=42#x";
        let scrubbed = scrub_url_query(raw);
        assert!(!scrubbed.contains("dsid"));
        assert!(!scrubbed.contains("accessKey"));
        assert!(!scrubbed.contains("secret"));
        assert!(!scrubbed.contains("1234567890"));
        assert_eq!(scrubbed, "https://mvod.example.com/P1/audio.m3u8");
    }

    #[test]
    fn scrub_url_query_falls_back_on_unparseable_input() {
        assert_eq!(scrub_url_query("not a url"), "<unparseable-url>");
    }

    /// `fetch_playback` parsing path: music-video webPlayback body with
    /// `assets: []` and `hls-playlist-url` populated. The playback picker
    /// must return `Manifest::VideoMaster(url)` when the request is a music
    /// video, instead of failing on an empty asset list.
    #[test]
    fn parses_music_video_playback_response_into_video_master_manifest() {
        // Minimal shape, same key names as a captured response.
        let body = r#"{
            "songList": [{
                "hls-key-cert-url": "https://s.mzstatic.com/skdtool_2021_certbundle.bin",
                "hls-playlist-url": "https://play.itunes.apple.com/WebObjects/MZPlay.woa/hls/playlist.m3u8?cc=US&a=6782267818&dsid=X&id=1000000000&l=en&aec=HD",
                "assets": [],
                "widevine-cert-url": "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/widevineCert",
                "songId": "6782267818",
                "hls-key-server-url": "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense"
            }],
            "status": 0
        }"#;
        let resp: PlaybackResponse = serde_json::from_str(body).unwrap();
        let song = resp.song_list.unwrap().remove(0);
        assert!(song.assets.unwrap_or_default().is_empty());
        assert_eq!(
            song.hls_playlist_url.as_deref(),
            Some(
                "https://play.itunes.apple.com/WebObjects/MZPlay.woa/hls/playlist.m3u8?cc=US&a=6782267818&dsid=X&id=1000000000&l=en&aec=HD"
            )
        );
        assert!(song.widevine_cert_url.is_some());
        assert!(song.hls_key_server_url.is_some());
    }

    // --- spatial (enhancedHls binaural) ------------------------------------

    /// Excerpt of Creepin's enhancedHls master (public CDN playlist).
    const SPATIAL_MASTER: &str = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-stereo-256",AUTOSELECT=YES,CHANNELS="2",NAME="songEnhanced"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-stereo-256-binaural",AUTOSELECT=YES,CHANNELS="2/-/BINAURAL",NAME="songEnhanced"

#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=259051,BANDWIDTH=271654,CODECS="mp4a.40.2",AUDIO="audio-stereo-256-downmix"
P1_A2_audio_en_gr256_mp4a-40-2_dm.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=258415,BANDWIDTH=276794,CODECS="mp4a.40.2",AUDIO="audio-stereo-256"
P1_A2_audio_en_gr256_mp4a-40-2.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=258466,BANDWIDTH=274228,CODECS="mp4a.40.2",AUDIO="audio-stereo-256-binaural"
P1_A2_audio_en_gr256_mp4a-40-2_bm.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=131604,BANDWIDTH=140887,CODECS="mp4a.40.2",AUDIO="audio-stereo-128-binaural"
P1_A2_audio_en_gr128_mp4a-40-2_bm.m3u8
"#;

    /// The binaural media playlist's layout: `s1/e1` for segment 0, `c24`
    /// for the rest (`FairPlay` and `PlayReady` lines around each).
    const SPATIAL_MEDIA: &str = r#"#EXTM3U
#EXT-X-TARGETDURATION:15
#EXT-X-VERSION:7
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="skd://itunes.apple.com/P000000000/s1/e1",KEYFORMAT="com.apple.streamingkeydelivery",KEYFORMATVERSIONS="1"
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="data:text/plain;charset=UTF-16;base64,vgEAAAEAAQC0AQ==",KEYFORMAT="com.microsoft.playready",KEYFORMATVERSIONS="1"
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="data:text/plain;base64,AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAAAAAAAAczEvZTEgICBI88aJmwY=",KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed",KEYFORMATVERSIONS="1"
#EXT-X-MAP:URI="P1_A2_bm_m.mp4",BYTERANGE="1124@0"
#EXTINF:14.99733,
#EXT-X-BYTERANGE:488185@1124
P1_A2_bm_m.mp4
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="skd://itunes.apple.com/p1/c24",KEYFORMAT="com.apple.streamingkeydelivery",KEYFORMATVERSIONS="1"
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="data:text/plain;charset=UTF-16;base64,vgEAAAEAAQC0AQ==",KEYFORMAT="com.microsoft.playready",KEYFORMATVERSIONS="1"
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="data:text/plain;base64,AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAABKf/6YYzI0ICAgICBI88aJmwY=",KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed",KEYFORMATVERSIONS="1"
#EXTINF:14.99733,
#EXT-X-BYTERANGE:463771@489309
P1_A2_bm_m.mp4
#EXTINF:14.99733,
#EXT-X-BYTERANGE:474586@953080
P1_A2_bm_m.mp4
#EXT-X-ENDLIST
"#;

    const SPATIAL_BASE: &str = "https://aod.itunes.apple.com/itunes-assets/x/P1_A2_bm.m3u8";

    #[test]
    fn binaural_variant_should_be_picked_by_audio_group() {
        assert_eq!(
            pick_binaural_variant(SPATIAL_MASTER),
            Some("P1_A2_audio_en_gr256_mp4a-40-2_bm.m3u8")
        );
    }

    #[test]
    fn master_without_binaural_variant_should_yield_none() {
        let master = SPATIAL_MASTER.replace("audio-stereo-256-binaural", "audio-stereo-256-other");
        assert_eq!(pick_binaural_variant(&master), None);
    }

    #[test]
    fn spatial_playlist_should_split_segments_into_key_groups() {
        let hls = parse_spatial_playlist(SPATIAL_MEDIA, SPATIAL_BASE).unwrap();
        let ranges: Vec<_> = hls.key_groups.iter().map(|g| g.segments.clone()).collect();
        assert_eq!(ranges, [0..1, 1..3]);
    }

    #[test]
    fn spatial_key_groups_should_keep_the_widevine_uri_verbatim() {
        let hls = parse_spatial_playlist(SPATIAL_MEDIA, SPATIAL_BASE).unwrap();
        assert_eq!(
            hls.key_groups[1].key_uri,
            "data:text/plain;base64,AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAABKf/6YYzI0ICAgICBI88aJmwY="
        );
    }

    #[test]
    fn spatial_key_groups_should_carry_the_full_pssh_box() {
        let hls = parse_spatial_playlist(SPATIAL_MEDIA, SPATIAL_BASE).unwrap();
        let kids: Vec<Vec<u8>> = hls
            .key_groups
            .iter()
            .map(|g| g.pssh[34..50].to_vec())
            .collect();
        assert_eq!(
            kids,
            [
                b"\0\0\0\0\0\0\0\0s1/e1   ".to_vec(),
                b"\0\0\0\0\x4a\x7f\xfe\x98c24     ".to_vec()
            ]
        );
    }

    #[test]
    fn spatial_layout_should_resolve_init_and_byte_ranges() {
        let hls = parse_spatial_playlist(SPATIAL_MEDIA, SPATIAL_BASE).unwrap();
        let url = "https://aod.itunes.apple.com/itunes-assets/x/P1_A2_bm_m.mp4";
        assert_eq!(
            (hls.init_segment.clone(), hls.segments[2].clone()),
            (
                SegmentRange {
                    url: url.to_owned(),
                    byte_range: Some((0, 1124)),
                },
                SegmentRange {
                    url: url.to_owned(),
                    byte_range: Some((953_080, 474_586)),
                }
            )
        );
    }

    #[test]
    fn segment_before_any_widevine_key_should_fail() {
        let manifest = "#EXTM3U\n#EXT-X-MAP:URI=\"i.mp4\"\n#EXTINF:1,\na.mp4\n#EXT-X-ENDLIST\n";
        assert!(matches!(
            parse_key_groups(manifest, 1),
            Err(DrmError::MissingKeyUri)
        ));
    }
}

/// Wiremock-backed tests for the HTTP + error-mapping surface. Each test spins up a `MockServer`, points an [`Apple`] (or
/// a direct `reqwest` request for segment fetches) at the server's URI,
/// and asserts on the resulting [`DownloadError`] variant — not just on
/// the error's string form — so the typed error mapping is pinned.
#[cfg(test)]
mod wiremock_tests {
    use cider_offline_core::download::{DownloadError, MediaKind, PlaybackId};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::Apple;
    use crate::error::DrmError;

    fn apple_at(server: &MockServer) -> Apple {
        let client = reqwest::Client::new();
        Apple::new(client, "bearer-dev-token", "mut-token")
            .expect("tokens are valid header values")
            .with_web_playback(format!("{}/webPlayback", server.uri()))
    }

    fn song_catalog() -> PlaybackId {
        PlaybackId::catalog("1443063973")
    }

    #[tokio::test]
    async fn web_playback_200_returns_widevine_playback() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "songList": [{
                "assets": [{
                    "flavor": "28:ctrp256",
                    "URL": "https://aod-ssl.example.com/mzaf.m3u8",
                }],
                "widevine-cert-url": "https://play.example.com/wa/widevineCert",
                "hls-key-server-url": "https://play.example.com/wa/acquireWebPlaybackLicense",
            }],
            "status": 0,
        });
        Mock::given(method("POST"))
            .and(path("/webPlayback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let apple = apple_at(&server);
        let out = apple
            .fetch_playback(&song_catalog(), MediaKind::Song)
            .await
            .expect("200 webPlayback must parse");
        assert_eq!(out.cert_url, "https://play.example.com/wa/widevineCert");
    }

    #[tokio::test]
    async fn web_playback_401_maps_to_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webPlayback"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let err = apple_at(&server)
            .fetch_playback(&song_catalog(), MediaKind::Song)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DrmError::HttpStatus { status: 401, .. }),
            "got {err:?}"
        );
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::AuthExpired { .. }
        ));
    }

    #[tokio::test]
    async fn web_playback_429_with_retry_after_maps_to_transient() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webPlayback"))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("retry-after", "30")
                    .set_body_string("too many requests"),
            )
            .mount(&server)
            .await;

        let err = apple_at(&server)
            .fetch_playback(&song_catalog(), MediaKind::Song)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DrmError::HttpStatus {
                status: 429,
                retry_after: Some(d),
                ..
            } if d == std::time::Duration::from_secs(30)
        ));
        match DownloadError::from(err) {
            DownloadError::Transient {
                status,
                retry_after,
                ..
            } => {
                assert_eq!(status, Some(429));
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn widevine_cert_500_maps_to_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/widevineCert"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server exploded"))
            .mount(&server)
            .await;

        let apple = apple_at(&server);
        let err = apple
            .fetch_widevine_cert(&format!("{}/widevineCert", server.uri()))
            .await
            .unwrap_err();
        assert!(matches!(err, DrmError::HttpStatus { status: 500, .. }));
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::Transient { .. }
        ));
    }

    #[tokio::test]
    async fn license_200_empty_body_maps_to_transient() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/acquireWebPlaybackLicense"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;

        let apple = apple_at(&server);
        let err = apple
            .acquire_license(
                &format!("{}/acquireWebPlaybackLicense", server.uri()),
                "AAAA",
                "data:;base64,AAAA",
                &song_catalog(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DrmError::EmptyLicense { status: 200, .. }),
            "got {err:?}"
        );
        match DownloadError::from(err) {
            DownloadError::Transient { status, .. } => assert_eq!(status, Some(200)),
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn license_200_with_error_code_2002_maps_to_auth_expired() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "errorCode": 2002,
            "status": 0,
            "message": "Your session has ended.",
        });
        Mock::given(method("POST"))
            .and(path("/acquireWebPlaybackLicense"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let apple = apple_at(&server);
        let err = apple
            .acquire_license(
                &format!("{}/acquireWebPlaybackLicense", server.uri()),
                "AAAA",
                "data:;base64,AAAA",
                &song_catalog(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DrmError::SessionExpired), "got {err:?}");
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::AuthExpired { .. }
        ));
    }

    /// Segment-fetch 403: the orchestrator's `fetch_url_plan` must map a
    /// 403 from the aod-ssl CDN to `HttpStatus { status: 403 }`, which
    /// `From<DrmError>` flips to `AuthExpired`. Exercised end-to-end via a
    /// direct `reqwest::get` + status check, the same
    /// way the orchestrator fetches segments.
    #[tokio::test]
    async fn segment_403_maps_to_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/segment"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/segment", server.uri()))
            .send()
            .await
            .expect("transport ok");
        let status = resp.status();
        assert_eq!(status.as_u16(), 403);
        let err = DrmError::HttpStatus {
            endpoint: "segment",
            status: status.as_u16(),
            body_len: 0,
            retry_after: None,
        };
        assert!(matches!(
            DownloadError::from(err),
            DownloadError::AuthExpired { .. }
        ));
    }
}
