//! Netease Cloud Music lyrics through its public, unauthenticated web API
//! (unofficial: it may change or stop working).
//!
//! 1. `GET /api/search/get?s=<artist title>&type=1&limit=10` lists songs
//!    (`result.songs[]`: `id`, `name`, `artists[].name`, `duration` in ms).
//! 2. `GET /api/song/lyric/v1?id=<id>&lv=-1&yv=-1&tv=-1` returns `yrc.lyric`
//!    (word-timed: `[start,dur](wordStart,wordDur,0)word…`, absolute ms) and
//!    `lrc.lyric` (LRC). `nolyric` / `pureMusic` mean none.

use async_trait::async_trait;
use serde::Deserialize;

use super::convert::{from_lrc, from_words};
use super::matching::{Candidate, best};
use super::ttml::{TimedLine, TimedWord};
use super::{
    FetchedLyrics, LyricsError, LyricsProvider, LyricsProviderId, LyricsQuality, LyricsRequest,
    ProviderError, web,
};

/// Public Netease API host.
const BASE_URL: &str = "https://music.163.com";
/// Search results considered.
const SEARCH_LIMIT: &str = "10";

#[derive(Debug, Deserialize)]
struct SearchResponse {
    code: i64,
    #[serde(default)]
    result: Option<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    #[serde(default)]
    songs: Vec<Song>,
}

#[derive(Debug, Deserialize)]
struct Song {
    id: u64,
    name: String,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    duration: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Artist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct LyricResponse {
    code: i64,
    #[serde(default)]
    nolyric: bool,
    #[serde(default, rename = "pureMusic")]
    pure_music: bool,
    #[serde(default)]
    yrc: Option<Lyric>,
    #[serde(default)]
    lrc: Option<Lyric>,
}

#[derive(Debug, Deserialize)]
struct Lyric {
    #[serde(default)]
    lyric: String,
}

/// Maps Netease's in-body `code`: `200` is fine, `-460` / `-462` are its
/// anti-abuse blocks (retry later), anything else is fatal.
fn check_code(code: i64) -> Result<(), ProviderError> {
    match code {
        200 => Ok(()),
        -460 | -462 | 405 | 429 => Err(ProviderError::Retryable {
            reason: format!("NetEase is limiting requests (code {code})"),
            retry_after: None,
        }),
        other => Err(ProviderError::Fatal(format!(
            "NetEase answered code {other}"
        ))),
    }
}

/// Parses a `(start,duration[,flag])` tag at the start of `text`; returns
/// the times and the tag's length in bytes.
fn timing_triple(text: &str, open: char, close: char) -> Option<(u64, u64, usize)> {
    let inner = text.strip_prefix(open)?;
    let end = inner.find(close)?;
    // `[start,duration]` (line) or `(start,duration,0)` (word).
    let mut parts = inner[..end].split(',');
    let start = parts.next()?.trim().parse().ok()?;
    let duration = parts.next()?.trim().parse().ok()?;
    if let Some(flag) = parts.next()
        && flag.trim().parse::<i64>().is_err()
    {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((start, duration, end + 2))
}

/// Splits `body` into words introduced by `(start,dur,0)`-style tags
/// (`open` / `close` are the tag brackets). `base` is added to each start
/// (KRC word times are relative to the line).
pub(crate) fn parse_word_tags(body: &str, open: char, close: char, base: u64) -> Vec<TimedWord> {
    let mut words: Vec<TimedWord> = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        if let Some((start, duration, used)) = timing_triple(rest, open, close) {
            words.push(TimedWord {
                start_ms: base + start,
                end_ms: base + start + duration,
                text: String::new(),
            });
            rest = &rest[used..];
            continue;
        }
        let mut chars = rest.chars();
        let Some(c) = chars.next() else { break };
        if let Some(word) = words.last_mut() {
            word.text.push(c);
        }
        rest = chars.as_str();
    }
    words
}

/// Parses Netease YRC into word-timed lines (JSON credit lines skipped).
pub(crate) fn parse_yrc(yrc: &str) -> Vec<TimedLine> {
    let mut lines = Vec::new();
    for raw in yrc.lines().map(str::trim) {
        let Some((start, duration, used)) = timing_triple(raw, '[', ']') else {
            continue;
        };
        let words = parse_word_tags(&raw[used..], '(', ')', 0);
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

/// Netease Cloud Music provider (see the module docs).
#[derive(Debug, Clone)]
pub struct NeteaseProvider {
    http: reqwest::Client,
    base_url: String,
}

impl NeteaseProvider {
    /// Provider for the public API.
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
impl LyricsProvider for NeteaseProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Netease
    }

    fn best_quality(&self) -> LyricsQuality {
        LyricsQuality::Word
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let query = request.query;
        let keywords = format!("{} {}", query.artist, query.title);
        let search = self
            .http
            .get(format!("{}/api/search/get", self.base_url))
            .query(&[
                ("s", keywords.as_str()),
                ("type", "1"),
                ("limit", SEARCH_LIMIT),
            ]);
        let Some(found) = web::json::<SearchResponse>(search).await? else {
            return Ok(None);
        };
        check_code(found.code)?;
        let songs = found.result.map(|r| r.songs).unwrap_or_default();
        let artists: Vec<Vec<&str>> = songs
            .iter()
            .map(|s| s.artists.iter().map(|a| a.name.as_str()).collect())
            .collect();
        let Some(song) = best(
            query,
            songs.iter().zip(&artists).map(|(song, artists)| {
                (
                    song,
                    Candidate {
                        title: &song.name,
                        artists,
                        duration_ms: song.duration,
                    },
                )
            }),
        ) else {
            return Ok(None);
        };
        let matched = format!(
            "NetEase {}: {} – {} ({} s)",
            song.id,
            song.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            song.name,
            song.duration.unwrap_or(0) / 1_000
        );
        let id = song.id.to_string();
        let lyric = self
            .http
            .get(format!("{}/api/song/lyric/v1", self.base_url))
            .query(&[
                ("id", id.as_str()),
                ("lv", "-1"),
                ("yv", "-1"),
                ("tv", "-1"),
            ]);
        let Some(lyric) = web::json::<LyricResponse>(lyric).await? else {
            return Ok(None);
        };
        check_code(lyric.code)?;
        Ok(convert(lyric, request, matched))
    }
}

/// The best lyrics in a lyric response: YRC (word) first, then LRC.
fn convert(
    lyric: LyricResponse,
    request: &LyricsRequest<'_>,
    matched: String,
) -> Option<FetchedLyrics> {
    if lyric.nolyric || lyric.pure_music {
        return None;
    }
    if let Some(yrc) = lyric.yrc.filter(|y| !y.lyric.trim().is_empty())
        && let Some(found) = from_words(parse_yrc(&yrc.lyric), request.query, matched.clone())
    {
        return Some(found);
    }
    lyric
        .lrc
        .filter(|l| !l.lyric.trim().is_empty())
        .and_then(|lrc| from_lrc(&lrc.lyric, request.query, matched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::web::test_server::serve;
    use crate::lyrics::{LyricsQuery, ttml_quality};

    const YRC: &str = "{\"t\":0,\"c\":[{\"tx\":\"制作人: \"},{\"tx\":\"The Weeknd\"}]}\n\
        [27360,1290](27360,240,0)I've (27600,90,0)been (27690,360,0)tryna (28050,600,0)call\n\
        [32820,4800](32820,540,0)Maybe (33360,120,0)you (34620,120,0)(yeah)(34740,900,0)love";

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
    fn yrc_should_parse_absolute_word_times() {
        let lines = parse_yrc(YRC);
        assert_eq!(
            (lines.len(), lines[0].words[1].clone()),
            (
                2,
                TimedWord {
                    start_ms: 27_600,
                    end_ms: 27_690,
                    text: "been ".to_owned()
                }
            )
        );
    }

    #[test]
    fn parentheses_in_text_should_stay_text() {
        let lines = parse_yrc(YRC);
        assert_eq!(lines[1].words[2].text, "(yeah)");
    }

    #[test]
    fn yrc_should_become_word_ttml() {
        let lyric = LyricResponse {
            code: 200,
            nolyric: false,
            pure_music: false,
            yrc: Some(Lyric {
                lyric: YRC.to_owned(),
            }),
            lrc: None,
        };
        let q = query();
        let request = LyricsRequest {
            catalog_id: None,
            query: &q,
        };
        let found = convert(lyric, &request, String::new()).unwrap();
        assert_eq!(ttml_quality(&found.ttml), LyricsQuality::Word);
    }

    #[test]
    fn pure_music_should_be_none() {
        let lyric: LyricResponse = serde_json::from_str(
            r#"{"code":200,"pureMusic":true,"lrc":{"lyric":"[00:00.00]纯音乐，请欣赏"}}"#,
        )
        .unwrap();
        let q = query();
        let request = LyricsRequest {
            catalog_id: None,
            query: &q,
        };
        assert_eq!(convert(lyric, &request, String::new()), None);
    }

    #[test]
    fn anti_abuse_code_should_be_retryable() {
        assert!(check_code(-460).unwrap_err().is_retryable());
    }

    async fn lookup_with(replies: Vec<(u16, &'static str, String)>) -> Option<FetchedLyrics> {
        let base = serve(replies).await;
        let provider = NeteaseProvider::with_base_url(&base).unwrap();
        let q = query();
        provider
            .lookup(&LyricsRequest {
                catalog_id: None,
                query: &q,
            })
            .await
            .unwrap()
    }

    fn search_body(name: &str, artist: &str, duration: u64) -> String {
        serde_json::json!({"code": 200, "result": {"songs": [
            {"id": 1, "name": name, "artists": [{"name": artist}], "duration": duration}
        ]}})
        .to_string()
    }

    #[tokio::test]
    async fn matching_song_should_return_its_lyrics() {
        let lyric = serde_json::json!({"code": 200, "yrc": {"lyric": YRC}}).to_string();
        let found = lookup_with(vec![
            (
                200,
                "",
                search_body("Blinding Lights", "The Weeknd", 200_045),
            ),
            (200, "", lyric),
        ])
        .await;
        assert_eq!(found.map(|f| f.quality), Some(LyricsQuality::Word));
    }

    #[tokio::test]
    async fn wrong_song_should_be_rejected_without_fetching_lyrics() {
        let found = lookup_with(vec![(
            200,
            "",
            search_body("Blinding Lights", "Some Cover Band", 200_045),
        )])
        .await;
        assert_eq!(found, None);
    }
}
