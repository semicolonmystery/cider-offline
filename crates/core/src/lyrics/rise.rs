//! Cider's own lyrics backend ("Rise", `exp-rise.cider.sh/api/v1/lyrics`),
//! asked exactly as the Cider client asks it: no account, no token.
//!
//! - **Musixmatch**: `POST /mxm` with
//!   `{"id", "name", "artist", "album", "duration" (s), "richSync": true}`.
//!   The backend matches the song on Musixmatch itself and answers
//!   `{"type": "Word" | "Line", "body", "syllableBody", "lineBody", …}`:
//!   for `Word` the body is Musixmatch richsync JSON
//!   (`[{"ts", "te", "x", "l": [{"c", "o"}]}]`, seconds, word offsets
//!   relative to the line), for `Line` (and `lineBody`) a line-timed TTML
//!   document. `404` means no lyrics.
//! - **Community** ("User Submitted" in Cider): `GET /user/<catalog id>`
//!   answers `{"ttml", "syncType", …}` for lyrics Cider users contributed,
//!   or `{"code": "NOT_FOUND"}`.
//!
//! Both are keyed by the Apple Music catalog id (the backend caches by
//! it), so tracks without one are skipped.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::amll::clean_ttml;
use super::convert::from_words;
use super::ttml::{TimedLine, TimedWord, parse_clock};
use super::{
    FetchedLyrics, LyricsError, LyricsKind, LyricsProvider, LyricsProviderId, LyricsQuality,
    LyricsQuery, LyricsRequest, ProviderError, USER_AGENT, ttml_quality, web,
};

/// Cider's lyrics API.
const BASE_URL: &str = "https://exp-rise.cider.sh/api/v1/lyrics";
/// How long to wait for `/mxm`. A song the backend has not cached yet
/// stalls for minutes (Cloudflare gives up with `524` after ~125 s), but
/// the backend finishes the lookup anyway and answers at once on a later
/// try, so waiting longer only blocks the queue.
const MXM_TIMEOUT: Duration = Duration::from_secs(20);
/// Suggested pause before asking again about a song still being looked up.
const MXM_RETRY_AFTER: Duration = Duration::from_mins(1);
/// Lyrics whose last line starts this long after the track ends belong to
/// another recording.
const OVERRUN_TOLERANCE_MS: u64 = 10_000;

/// A catalog id usable as the backend's key.
fn catalog_id<'a>(request: &LyricsRequest<'a>) -> Option<&'a str> {
    request
        .catalog_id
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

fn base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_owned()
}

/// A Rise TTML document made Apple-shaped (no XML prolog, `itunes:timing`,
/// a `<head>`); `None` when it is not TTML or has no text.
fn tidy_ttml(raw: &str) -> Option<String> {
    let mut text = raw.trim();
    if let Some(rest) = text.strip_prefix("<?xml") {
        text = rest.split_once("?>")?.1.trim_start();
    }
    if !text.starts_with("<tt") {
        return None;
    }
    let mut ttml = clean_ttml(text);
    if !ttml.contains("<head") {
        let open_end = ttml.find('>')?;
        ttml.insert_str(open_end + 1, "<head><metadata/></head>");
    }
    has_text(&ttml).then_some(ttml)
}

/// Whether any `<p>` of a TTML document holds text.
fn has_text(ttml: &str) -> bool {
    ttml.split("<p").skip(1).any(|p| {
        // The split starts inside the `<p …>` tag.
        let mut inside_tag = true;
        p.split("</p>").next().unwrap_or_default().chars().any(|c| {
            match c {
                '<' => inside_tag = true,
                '>' => inside_tag = false,
                c if !inside_tag && !c.is_whitespace() => return true,
                _ => {}
            }
            false
        })
    })
}

/// Latest `<p begin>` of a TTML document, in milliseconds.
fn last_line_start(ttml: &str) -> Option<u64> {
    ttml.split("<p begin=\"")
        .skip(1)
        .filter_map(|rest| parse_clock(rest.split('"').next()?))
        .max()
}

/// Whether lyrics whose last line starts at `last_start_ms` can belong to
/// the track (unknown durations pass).
fn fits(query: &LyricsQuery, last_start_ms: Option<u64>) -> bool {
    match (
        query.duration_ms.map(u64::from).filter(|d| *d > 0),
        last_start_ms,
    ) {
        (Some(duration), Some(last)) => last <= duration + OVERRUN_TOLERANCE_MS,
        _ => true,
    }
}

/// TTML lyrics as [`FetchedLyrics`], when they fit the track.
fn from_ttml(raw: &str, query: &LyricsQuery, matched: String) -> Option<FetchedLyrics> {
    let ttml = tidy_ttml(raw)?;
    if !fits(query, last_line_start(&ttml)) {
        return None;
    }
    Some(FetchedLyrics {
        quality: ttml_quality(&ttml),
        ttml,
        kind: LyricsKind::Line,
        matched: Some(matched),
    })
}

#[derive(Debug, Serialize)]
struct MxmRequest<'a> {
    id: &'a str,
    name: &'a str,
    artist: &'a str,
    album: &'a str,
    /// Whole seconds.
    duration: u64,
    #[serde(rename = "richSync")]
    rich_sync: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MxmResponse {
    /// Error code sent with HTTP 200 (`INVALID_REQUEST`, `NOT_FOUND`).
    code: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    body: Option<String>,
    #[serde(rename = "syllableBody")]
    syllable_body: Option<String>,
    #[serde(rename = "lineBody")]
    line_body: Option<String>,
}

/// One richsync line: start / end in seconds, words at offsets from `ts`.
#[derive(Debug, Deserialize)]
struct RichLine {
    ts: f64,
    te: f64,
    #[serde(default)]
    l: Vec<RichChar>,
}

#[derive(Debug, Deserialize)]
struct RichChar {
    c: String,
    o: f64,
}

/// Seconds to milliseconds (negative and absurd values clamped).
fn millis(seconds: f64) -> u64 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=1e12 ms first, which u64 holds exactly enough"
    )]
    let ms = (seconds.clamp(0.0, 1e9) * 1_000.0).round() as u64;
    ms
}

/// Parses Musixmatch richsync JSON into word-timed lines. The whitespace
/// entries between words become trailing spaces of the word before.
fn parse_richsync(json: &str) -> Option<Vec<TimedLine>> {
    let raw: Vec<RichLine> = serde_json::from_str(json).ok()?;
    let lines = raw
        .into_iter()
        .filter_map(|line| {
            let start = millis(line.ts);
            let end = millis(line.te).max(start);
            let mut words: Vec<TimedWord> = Vec::new();
            for (index, part) in line.l.iter().enumerate() {
                if part.c.trim().is_empty() {
                    if let Some(word) = words.last_mut() {
                        word.text.push(' ');
                    }
                    continue;
                }
                let word_start = (start + millis(part.o)).min(end);
                let word_end = line.l[index + 1..]
                    .first()
                    .map_or(end, |next| start + millis(next.o))
                    .clamp(word_start, end);
                words.push(TimedWord {
                    start_ms: word_start,
                    end_ms: word_end,
                    text: part.c.clone(),
                });
            }
            (!words.is_empty()).then_some(TimedLine {
                start_ms: start,
                end_ms: end,
                words,
            })
        })
        .collect();
    Some(lines)
}

/// The best lyrics in a `/mxm` answer: richsync, else the line TTML.
fn convert_mxm(
    response: &MxmResponse,
    query: &LyricsQuery,
    matched: &str,
) -> Option<FetchedLyrics> {
    if response.kind == "Word"
        && let Some(lines) = [&response.syllable_body, &response.body]
            .into_iter()
            .flatten()
            .find_map(|body| parse_richsync(body))
        && fits(query, lines.iter().map(|l| l.start_ms).max())
        && let Some(found) = from_words(lines, query, matched.to_owned())
    {
        return Some(found);
    }
    [&response.line_body, &response.body]
        .into_iter()
        .flatten()
        .find_map(|body| from_ttml(body, query, matched.to_owned()))
}

/// Musixmatch lyrics through Cider's backend (see the module docs).
///
/// A song the backend has no Musixmatch lyrics for may stall on every
/// try instead of answering `404`. The first stall of a song is a
/// retryable failure (the usual "still looking it up" case: the next try
/// is answered from the backend's cache); a second stall of the same song
/// in this run counts as no lyrics.
#[derive(Debug)]
pub struct MusixmatchProvider {
    http: reqwest::Client,
    base_url: String,
    stalled: Mutex<HashSet<String>>,
}

impl MusixmatchProvider {
    /// Provider for Cider's public backend.
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
        Self::with_timeout(base_url, MXM_TIMEOUT)
    }

    fn with_timeout(base_url: &str, timeout: Duration) -> Result<Self, LyricsError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(timeout)
                .build()?,
            base_url: self::base_url(base_url),
            stalled: Mutex::new(HashSet::new()),
        })
    }
}

#[async_trait]
impl LyricsProvider for MusixmatchProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Musixmatch
    }

    fn best_quality(&self) -> LyricsQuality {
        LyricsQuality::Word
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let Some(id) = catalog_id(request) else {
            return Ok(None);
        };
        let query = request.query;
        // The backend rejects an empty album or a zero duration
        // (`INVALID_REQUEST`); Cider always has both.
        let duration = query
            .duration_ms
            .map_or(0, |ms| (u64::from(ms) + 500) / 1_000);
        if duration == 0 || query.title.trim().is_empty() || query.artist.trim().is_empty() {
            return Ok(None);
        }
        let name = strip_release_suffix(&query.title);
        let album = query
            .album
            .as_deref()
            .map(strip_release_suffix)
            .filter(|album| !album.is_empty())
            .unwrap_or(name);
        let body = MxmRequest {
            id,
            name,
            artist: query.artist.trim(),
            album,
            duration,
            rich_sync: true,
        };
        let post = self.http.post(format!("{}/mxm", self.base_url)).json(&body);
        let response = match web::json::<MxmResponse>(post).await {
            Ok(Some(response)) => response,
            Ok(None) => return Ok(None),
            // `from_reqwest`'s reason for a timeout.
            Err(ProviderError::Retryable { reason, .. }) if reason == "timed out" => {
                let first_stall = self
                    .stalled
                    .lock()
                    .map_or(true, |mut stalled| stalled.insert(id.to_owned()));
                if !first_stall {
                    tracing::debug!(id, "Musixmatch lookup stalled again; treating as no lyrics");
                    return Ok(None);
                }
                return Err(ProviderError::Retryable {
                    reason: "Cider's lyrics service is still looking this song up".to_owned(),
                    retry_after: Some(MXM_RETRY_AFTER),
                });
            }
            Err(error) => return Err(error),
        };
        match response.code.as_deref() {
            None => {}
            Some("NOT_FOUND") => return Ok(None),
            Some(code) => {
                return Err(ProviderError::Fatal(format!(
                    "Cider's lyrics service answered {code}"
                )));
            }
        }
        Ok(convert_mxm(
            &response,
            query,
            &format!("Musixmatch via Cider (id {id})"),
        ))
    }
}

/// Drops the `" - Single"` / `" - EP"` release suffixes, as Cider does
/// before asking.
fn strip_release_suffix(name: &str) -> &str {
    let name = name.trim();
    [" - Single", " - EP"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name)
        .trim_end()
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UserLyrics {
    code: Option<String>,
    ttml: Option<String>,
}

/// Lyrics contributed by Cider users (see the module docs).
#[derive(Debug, Clone)]
pub struct CommunityProvider {
    http: reqwest::Client,
    base_url: String,
}

impl CommunityProvider {
    /// Provider for Cider's public backend.
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
            base_url: self::base_url(base_url),
        })
    }
}

#[async_trait]
impl LyricsProvider for CommunityProvider {
    fn id(&self) -> LyricsProviderId {
        LyricsProviderId::Community
    }

    fn best_quality(&self) -> LyricsQuality {
        LyricsQuality::Word
    }

    async fn lookup(
        &self,
        request: &LyricsRequest<'_>,
    ) -> Result<Option<FetchedLyrics>, ProviderError> {
        let Some(id) = catalog_id(request) else {
            return Ok(None);
        };
        let get = self.http.get(format!("{}/user/{id}", self.base_url));
        let Some(found) = web::json::<UserLyrics>(get).await? else {
            return Ok(None);
        };
        if found.code.as_deref() == Some("NOT_FOUND") {
            return Ok(None);
        }
        Ok(found.ttml.and_then(|ttml| {
            from_ttml(
                &ttml,
                request.query,
                format!("Cider community lyrics (id {id})"),
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::web::test_server::serve;

    const LINE_TTML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><tt xml:lang="en" xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body><div><p begin="00:00:13.140" end="00:00:27.100">Yeah</p><p begin="00:00:27.100" end="00:00:29.960">I&#39;ve been tryna call</p></div></body></tt>"#;

    const RICHSYNC: &str = r#"[{"ts":27.1,"te":29.96,"l":[{"c":"I've","o":0},{"c":" ","o":0.3},{"c":"been","o":0.4},{"c":" ","o":0.7},{"c":"tryna","o":0.8},{"c":" ","o":1.2},{"c":"call","o":1.3}],"x":"I've been tryna call"}]"#;

    fn query() -> LyricsQuery {
        LyricsQuery {
            title: "Blinding Lights".to_owned(),
            artist: "The Weeknd".to_owned(),
            album: Some("After Hours".to_owned()),
            duration_ms: Some(200_040),
            isrc: None,
        }
    }

    fn request(q: &LyricsQuery) -> LyricsRequest<'_> {
        LyricsRequest {
            catalog_id: Some("1488408568"),
            query: q,
        }
    }

    #[test]
    fn rise_line_ttml_should_become_apple_shaped_line_lyrics() {
        let found = from_ttml(LINE_TTML, &query(), String::new()).unwrap();
        assert!(
            found.quality == LyricsQuality::Line
                && found.ttml.contains("itunes:timing=\"Line\"")
                && found.ttml.contains("<head><metadata/></head>")
                && found
                    .ttml
                    .contains("<p begin=\"13.140\" end=\"27.100\">Yeah</p>")
        );
    }

    #[test]
    fn richsync_should_keep_word_times_and_spacing() {
        let lines = parse_richsync(RICHSYNC).unwrap();
        assert_eq!(
            lines[0].words[1],
            TimedWord {
                start_ms: 27_500,
                end_ms: 27_800,
                text: "been ".to_owned(),
            }
        );
    }

    #[test]
    fn word_answer_should_be_word_ttml() {
        let response = MxmResponse {
            kind: "Word".to_owned(),
            body: Some(RICHSYNC.to_owned()),
            line_body: Some(LINE_TTML.to_owned()),
            ..MxmResponse::default()
        };
        let found = convert_mxm(&response, &query(), "").unwrap();
        assert_eq!(ttml_quality(&found.ttml), LyricsQuality::Word);
    }

    #[test]
    fn broken_richsync_should_fall_back_to_line_body() {
        let response = MxmResponse {
            kind: "Word".to_owned(),
            body: Some("not json".to_owned()),
            line_body: Some(LINE_TTML.to_owned()),
            ..MxmResponse::default()
        };
        let found = convert_mxm(&response, &query(), "").unwrap();
        assert_eq!(found.quality, LyricsQuality::Line);
    }

    #[test]
    fn lyrics_far_longer_than_the_track_should_be_rejected() {
        let short = LyricsQuery {
            duration_ms: Some(10_000),
            ..query()
        };
        assert_eq!(from_ttml(LINE_TTML, &short, String::new()), None);
    }

    #[test]
    fn empty_ttml_should_be_none() {
        let empty = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.000" end="2.000"> </p></div></body></tt>"#;
        assert_eq!(from_ttml(empty, &query(), String::new()), None);
    }

    #[test]
    fn release_suffix_should_be_dropped() {
        assert_eq!(strip_release_suffix("After Hours - Single"), "After Hours");
    }

    #[tokio::test]
    async fn musixmatch_lookup_should_return_line_lyrics() {
        let body =
            serde_json::json!({"type": "Line", "syncType": "line", "body": LINE_TTML}).to_string();
        let base = serve(vec![(200, "", body)]).await;
        let provider = MusixmatchProvider::with_base_url(&base).unwrap();
        let q = query();
        let found = provider.lookup(&request(&q)).await.unwrap();
        assert_eq!(found.map(|f| f.quality), Some(LyricsQuality::Line));
    }

    #[tokio::test]
    async fn musixmatch_not_found_should_be_none() {
        let base = serve(vec![(404, "", "{}".to_owned())]).await;
        let provider = MusixmatchProvider::with_base_url(&base).unwrap();
        let q = query();
        assert_eq!(provider.lookup(&request(&q)).await, Ok(None));
    }

    /// A server that accepts connections and never answers.
    async fn silent_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn first_stall_should_be_retryable() {
        let base = silent_server().await;
        let provider = MusixmatchProvider::with_timeout(&base, Duration::from_millis(200)).unwrap();
        let q = query();
        let found = provider.lookup(&request(&q)).await;
        assert!(found.is_err_and(|e| e.is_retryable()));
    }

    #[tokio::test]
    async fn second_stall_of_a_song_should_be_no_lyrics() {
        let base = silent_server().await;
        let provider = MusixmatchProvider::with_timeout(&base, Duration::from_millis(200)).unwrap();
        let q = query();
        let _ = provider.lookup(&request(&q)).await;
        assert_eq!(provider.lookup(&request(&q)).await, Ok(None));
    }

    #[tokio::test]
    async fn invalid_request_code_should_be_fatal() {
        let body = r#"{"code":"INVALID_REQUEST","error":"Invalid request body"}"#.to_owned();
        let base = serve(vec![(200, "", body)]).await;
        let provider = MusixmatchProvider::with_base_url(&base).unwrap();
        let q = query();
        let found = provider.lookup(&request(&q)).await;
        assert!(found.is_err_and(|e| !e.is_retryable()));
    }

    #[tokio::test]
    async fn track_without_duration_should_not_be_looked_up() {
        let provider = MusixmatchProvider::with_base_url("http://127.0.0.1:9").unwrap();
        let q = LyricsQuery {
            duration_ms: None,
            ..query()
        };
        assert_eq!(provider.lookup(&request(&q)).await, Ok(None));
    }

    #[tokio::test]
    async fn track_without_catalog_id_should_not_be_looked_up() {
        let provider = MusixmatchProvider::with_base_url("http://127.0.0.1:9").unwrap();
        let q = query();
        let found = provider
            .lookup(&LyricsRequest {
                catalog_id: None,
                query: &q,
            })
            .await;
        assert_eq!(found, Ok(None));
    }

    #[tokio::test]
    async fn community_not_found_code_should_be_none() {
        let body = r#"{"code":"NOT_FOUND","error":"No user lyrics found"}"#.to_owned();
        let base = serve(vec![(200, "", body)]).await;
        let provider = CommunityProvider::with_base_url(&base).unwrap();
        let q = query();
        assert_eq!(provider.lookup(&request(&q)).await, Ok(None));
    }

    #[tokio::test]
    async fn community_ttml_should_be_returned() {
        let body = serde_json::json!({"ttml": LINE_TTML, "syncType": "line"}).to_string();
        let base = serve(vec![(200, "", body)]).await;
        let provider = CommunityProvider::with_base_url(&base).unwrap();
        let q = query();
        let found = provider.lookup(&request(&q)).await.unwrap();
        assert_eq!(found.map(|f| f.quality), Some(LyricsQuality::Line));
    }
}
