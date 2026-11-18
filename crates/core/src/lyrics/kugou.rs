//! Kugou lyrics through its public, unauthenticated lyrics service
//! (unofficial: it may change or stop working).
//!
//! 1. `GET lyrics.kugou.com/search?ver=1&man=yes&client=pc&keyword=<artist -
//!    title>&duration=<ms>` lists `candidates[]` (`id`, `accesskey` (a
//!    per-result download key, not a credential), `song`, `singer`,
//!    `duration` in ms).
//! 2. `GET …/download?ver=1&client=pc&id&accesskey&fmt=krc&charset=utf8`
//!    returns `content`: base64 KRC. KRC is the 4-byte magic `krc1`, then a
//!    zlib stream XOR-ed with a fixed 16-byte key; inside, word-timed lines
//!    `[lineStart,lineDur]<offset,dur,0>word…` with word offsets relative to
//!    the line.

use std::io::Read as _;

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;

use super::convert::from_words;
use super::matching::{Candidate, best};
use super::netease::parse_word_tags;
use super::ttml::TimedLine;
use super::{
    FetchedLyrics, LyricsError, LyricsProvider, LyricsProviderId, LyricsQuality, LyricsRequest,
    ProviderError, web,
};

/// Public Kugou lyrics host.
const BASE_URL: &str = "https://lyrics.kugou.com";
/// Magic prefix of a KRC file.
const KRC_MAGIC: &[u8] = b"krc1";
/// XOR key of the KRC format.
const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    status: i64,
    #[serde(default)]
    candidates: Vec<CandidateRecord>,
}

#[derive(Debug, Deserialize)]
struct CandidateRecord {
    id: String,
    accesskey: String,
    #[serde(default)]
    song: String,
    #[serde(default)]
    singer: String,
    #[serde(default)]
    duration: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DownloadResponse {
    #[serde(default)]
    status: i64,
    #[serde(default)]
    content: String,
}

/// Why a KRC file could not be read.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum KrcError {
    /// Not base64.
    #[error("KRC is not base64")]
    Base64,
    /// The `krc1` magic is missing.
    #[error("not a KRC file")]
    Magic,
    /// The zlib stream is broken or not UTF-8.
    #[error("KRC does not inflate")]
    Inflate,
}

/// Decodes base64 KRC into its text.
pub(crate) fn decode_krc(content: &str) -> Result<String, KrcError> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(content.trim())
        .map_err(|_| KrcError::Base64)?;
    let body = raw.strip_prefix(KRC_MAGIC).ok_or(KrcError::Magic)?;
    let unmasked: Vec<u8> = body
        .iter()
        .zip(KRC_KEY.iter().cycle())
        .map(|(byte, key)| byte ^ key)
        .collect();
    let mut text = String::new();
    flate2::read::ZlibDecoder::new(unmasked.as_slice())
        .read_to_string(&mut text)
        .map_err(|_| KrcError::Inflate)?;
    Ok(text)
}

/// Parses KRC text into word-timed lines (metadata tags skipped).
pub(crate) fn parse_krc(krc: &str) -> Vec<TimedLine> {
    let mut lines = Vec::new();
    for raw in krc.lines().map(str::trim) {
        let Some(inner) = raw.strip_prefix('[') else {
            continue;
        };
        let Some(close) = inner.find(']') else {
            continue;
        };
        let Some((start, duration)) = inner[..close].split_once(',') else {
            continue;
        };
        let (Ok(start), Ok(duration)) =
            (start.trim().parse::<u64>(), duration.trim().parse::<u64>())
        else {
            continue;
        };
        let words = parse_word_tags(&inner[close + 1..], '<', '>', start);
        if words.is_empty() {
            continue;
        }
        lines.push(TimedLine {
            start_ms: start,
            end_ms: start + duration,
            words,
        });
    }
    lines.sort_by_key(|line| line.start_ms);
    lines
}

/// Kugou provider (see the module docs).
#[derive(Debug, Clone)]
pub struct KugouProvider {
    http: reqwest::Client,
    base_url: String,
}

impl KugouProvider {
    /// Provider for the public service.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new() -> Result<Self, LyricsError> {
        Self::with_base_url(BASE_URL)
    }

    /// Provider for another host (tests).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn with_base_url(base_url: &str) -> Result<Self, LyricsError> {
        Ok(Self {
            http: web::client()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }
}

#[async_trait]
impl LyricsProvider for KugouProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Kugou
    }

    fn best_quality(&self) -> LyricsQuality {
        LyricsQuality::Word
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let query = request.query;
        let keyword = format!("{} - {}", query.artist, query.title);
        let duration = query.duration_ms.unwrap_or(0).to_string();
        let search = self.http.get(format!("{}/search", self.base_url)).query(&[
            ("ver", "1"),
            ("man", "yes"),
            ("client", "pc"),
            ("keyword", keyword.as_str()),
            ("duration", duration.as_str()),
            ("hash", ""),
        ]);
        let Some(found) = web::json::<SearchResponse>(search).await? else {
            return Ok(None);
        };
        if found.status != 200 {
            return Err(ProviderError::Retryable {
                reason: format!("Kugou search answered status {}", found.status),
                retry_after: None,
            });
        }
        let singers: Vec<[&str; 1]> = found
            .candidates
            .iter()
            .map(|c| [c.singer.as_str()])
            .collect();
        let Some(candidate) = best(
            query,
            found.candidates.iter().zip(&singers).map(|(c, singer)| {
                (
                    c,
                    Candidate {
                        title: &c.song,
                        artists: singer,
                        duration_ms: c.duration,
                    },
                )
            }),
        ) else {
            return Ok(None);
        };
        let matched = format!(
            "Kugou {}: {} – {} ({} s)",
            candidate.id,
            candidate.singer,
            candidate.song,
            candidate.duration.unwrap_or(0) / 1_000
        );
        let download = self
            .http
            .get(format!("{}/download", self.base_url))
            .query(&[
                ("ver", "1"),
                ("client", "pc"),
                ("id", candidate.id.as_str()),
                ("accesskey", candidate.accesskey.as_str()),
                ("fmt", "krc"),
                ("charset", "utf8"),
            ]);
        let Some(file) = web::json::<DownloadResponse>(download).await? else {
            return Ok(None);
        };
        if file.status != 200 || file.content.is_empty() {
            return Ok(None);
        }
        let krc = decode_krc(&file.content).map_err(|e| ProviderError::Fatal(e.to_string()))?;
        Ok(from_words(parse_krc(&krc), query, matched))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::lyrics::web::test_server::serve;
    use crate::lyrics::{LyricsQuery, ttml_quality};

    const KRC: &str = "[ti:Blinding Lights]\n[ar:The Weeknd]\n\
        [0,1756]<0,439,0>Blinding <439,439,0>Lights - <878,439,0>The <1317,439,0>Weeknd\n\
        [27672,2784]<0,200,0>I've <200,144,0>been <344,176,0>tryna <520,632,0>call\n";

    /// Encodes `text` the way Kugou serves KRC.
    fn encode_krc(text: &str) -> String {
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zlib.write_all(text.as_bytes()).unwrap();
        let packed = zlib.finish().unwrap();
        let mut raw = KRC_MAGIC.to_vec();
        raw.extend(
            packed
                .iter()
                .zip(KRC_KEY.iter().cycle())
                .map(|(b, k)| b ^ k),
        );
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    fn query() -> LyricsQuery {
        LyricsQuery {
            title: "Blinding Lights".to_owned(),
            artist: "The Weeknd".to_owned(),
            album: None,
            duration_ms: Some(200_040),
            isrc: None,
        }
    }

    #[test]
    fn krc_should_roundtrip_through_the_mask() {
        assert_eq!(decode_krc(&encode_krc(KRC)).as_deref(), Ok(KRC));
    }

    #[test]
    fn missing_magic_should_be_an_error() {
        let plain = base64::engine::general_purpose::STANDARD.encode(b"nope");
        assert_eq!(decode_krc(&plain), Err(KrcError::Magic));
    }

    #[test]
    fn krc_word_offsets_should_be_relative_to_the_line() {
        let lines = parse_krc(KRC);
        let word = &lines[1].words[1];
        assert_eq!((word.start_ms, word.end_ms), (27_872, 28_016));
    }

    #[tokio::test]
    async fn lookup_should_return_word_ttml_without_the_title_line() {
        let search = serde_json::json!({"status": 200, "candidates": [
            {"id": "9", "accesskey": "K", "song": "Blinding Lights", "singer": "The Weeknd", "duration": 200_097}
        ]})
        .to_string();
        let download = serde_json::json!({"status": 200, "content": encode_krc(KRC)}).to_string();
        let base = serve(vec![(200, "", search), (200, "", download)]).await;
        let provider = KugouProvider::with_base_url(&base).unwrap();
        let q = query();
        let found = provider
            .lookup(&LyricsRequest {
                catalog_id: None,
                query: &q,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            ttml_quality(&found.ttml) == LyricsQuality::Word
                && !found.ttml.contains("Weeknd")
                && found
                    .ttml
                    .contains("<span begin=\"27.872\" end=\"28.016\">been</span>")
        );
    }

    #[tokio::test]
    async fn no_candidates_should_be_none() {
        let base = serve(vec![(
            200,
            "",
            r#"{"status":200,"candidates":[]}"#.to_owned(),
        )])
        .await;
        let provider = KugouProvider::with_base_url(&base).unwrap();
        let q = query();
        let found = provider
            .lookup(&LyricsRequest {
                catalog_id: None,
                query: &q,
            })
            .await;
        assert_eq!(found, Ok(None));
    }
}
