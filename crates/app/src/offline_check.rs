//! Headless check of offline mode (`--offline-check`).
//!
//! Starts the real proxy core **forced offline** on an ephemeral port, over a
//! temporary copy of the real store (artwork, lyrics and media are hard-linked
//! or copied next to it, so nothing of the real data directory is modified),
//! and drives it exactly like Cider: through `CONNECT` + TLS with the proxy's
//! CA.
//!
//! ```text
//! cider-offline --offline-check [--captures <dir>] [--baseline] [--verbose]
//! ```
//!
//! 1. **Replay**: every distinct captured Cider request (method, URL, body;
//!    auth headers and cookies stripped) is replayed and checked per request
//!    pattern: status, content type, valid JSON:API (or image bytes), and the
//!    same shape Cider got online (key structure and value types of the
//!    captured online response for the same request).
//! 2. **Content**: every library playlist and album appears; every
//!    collection's tracks (paged by following the responder's `next` links
//!    the way Cider does) equal the synced membership in order; every artwork
//!    URL the responses reference resolves to real image bytes; search finds
//!    local songs, playlists and albums (accent-insensitive); starting playback
//!    from each collection queues exactly its downloaded tracks and each of
//!    them streams from the local media endpoint.
//! 3. **Variants** Cider plausibly sends offline but that were never captured
//!    (other offsets / limits, other collections, other artwork sizes and
//!    formats, catalog lookups for every local song).
//! 4. **Songs page**: before the proxy is used, the copy is synced from the
//!    live Apple Music API with the stored tokens; the offline Songs page then
//!    equals Apple's synced Songs tab (ids and order) with the "apple"
//!    setting, and every library song with "all".
//! 5. **Lyrics** (see [`lyrics`]): offline, every sampled song with local
//!    lyrics is served on both endpoints in Apple's shape; a song Apple says
//!    has no lyrics but that has local lyrics carries `hasLyrics` /
//!    `hasTimeSyncedLyrics: true` offline, online and in a simulated
//!    rewrite; custom lyrics win on both endpoints offline and online; online
//!    with "Always use downloaded lyrics" off Apple answers and is saved, on
//!    local lyrics are served without asking Apple.
//! 6. **Online**: the same proxy switched online. The Songs page passes
//!    through to Apple ("apple": Apple's live answer equals the synced list)
//!    or is answered locally ("all"); id lookups always go to Apple; playback
//!    of a downloaded track is answered with the local media URL (served with
//!    `Range`), of any other track forwarded to Apple untouched.
//!
//! `--baseline` instead evaluates the responses the proxy actually served Cider
//! during the captured offline sessions, with the
//! same per-pattern checks. Token values are never printed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, bail};
use cider_offline_core::config::ConnectivitySettings;
use cider_offline_core::storage::{CollectionKind, SyncedCollection, TokenKind};
use cider_offline_core::{AppPaths, Config, Mode, ProxyHandle, SongsPage, Storage};
use serde_json::Value;

mod lyrics;

/// The flag that selects this mode.
pub const FLAG: &str = "--offline-check";

/// Hosts whose captured requests are replayed.
const REPLAYED_HOSTS: &[&str] = &[
    "amp-api.music.apple.com",
    "is1-ssl.mzstatic.com",
    "play.itunes.apple.com",
    "universal-activity-service.itunes.apple.com",
    "p3-buy.itunes.apple.com",
];

/// Request headers never replayed (auth material, hop-by-hop, encodings).
const DROPPED_HEADERS: &[&str] = &[
    "host",
    "authority",
    "connection",
    "content-length",
    "authorization",
    "media-user-token",
    "music-user-token",
    "cookie",
    "accept-encoding",
    "proxy-connection",
    "range",
];

/// Attributes Apple returns online that cannot exist offline (editorial,
/// streaming and social data); their absence is reported, not failed.
const TOLERATED_ATTRIBUTES: &[&str] = &[
    "editorialArtwork",
    "editorialVideo",
    "editorialNotes",
    "offers",
    "extendedAssetUrls",
    "previews",
    "plainEditorialNotes",
    "plainEditorialCard",
    "followState",
    "tags",
    "credits",
    "fades",
    "personalRating",
    "lyricsExcerpt",
    "centeredFullscreenBackground",
    "artistBio",
    "bornOrFormed",
    "isGroup",
    "origin",
    "hero",
    "audioLocale",
    "audioTraits",
    "isAppleDigitalMaster",
    "isMasteredForItunes",
    "isVocalAttenuationAllowed",
    "hasCredits",
    "curatorName",
    "supportsSing",
    "isCompilation",
    "isComplete",
    "isSingle",
    "recordLabel",
    "copyright",
    "upc",
    "audioVariants",
    "isPrerelease",
    "workName",
    "movementName",
    "attribution",
    // Never fetched by the library sync (artist pages need the network anyway).
    "artistUrl",
    "curatorSocialHandle",
    // Catalog-only metadata of songs whose catalog twin the sync did not return.
    "composerName",
    // Optional per item (only explicit / clean items carry it).
    "contentRating",
    // Optional per item: Apple omits it for a playlist without a description
    // (compared against another playlist's capture when the same one has none).
    "description",
];

/// Relationships / views Apple returns online that have no local data.
const TOLERATED_RELATIONSHIPS: &[&str] = &[
    "artists",
    "audio-analysis",
    "genres",
    "music-videos",
    "record-labels",
    "curator",
    "composers",
    "credits",
    "lyrics",
    "station",
    "library",
    "catalog",
    "songs",
];

/// Options parsed from the command line.
#[derive(Debug, Default)]
pub struct Options {
    captures: Option<PathBuf>,
    baseline: bool,
    verbose: bool,
}

/// Returns the options when `--offline-check` is present.
pub fn parse(args: &[String]) -> Option<Options> {
    if !args.iter().any(|a| a == FLAG) {
        return None;
    }
    let mut options = Options::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--captures" => options.captures = iter.next().map(PathBuf::from),
            "--baseline" => options.baseline = true,
            "--verbose" => options.verbose = true,
            _ => {}
        }
    }
    Some(options)
}

/// Runs the check; `Ok(true)` when every assertion passed.
pub fn run(options: &Options) -> anyhow::Result<bool> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating the tokio runtime")?;
    rt.block_on(check(options))
}

/// Exits the process with the check's status (helper for `main`).
pub fn exit_with(result: anyhow::Result<bool>) -> anyhow::Result<()> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => bail!("offline check failed"),
        Err(error) => Err(error),
    }
}

// ----- Captures --------------------------------------------------------------

/// One captured exchange.
#[derive(Debug, Clone)]
struct Captured {
    file: String,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    served_locally: bool,
    status: Option<u16>,
    content_type: Option<String>,
    response: Option<String>,
}

impl Captured {
    fn parse(file: &str, value: &Value) -> Option<Self> {
        let request = value.get("request")?;
        let response = value.get("response");
        let header_list = |v: Option<&Value>| -> Vec<(String, String)> {
            v.and_then(|h| h.get("headers"))
                .and_then(Value::as_array)
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|p| {
                            Some((
                                p.get(0)?.as_str()?.to_owned(),
                                p.get(1)?.as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let response_headers = header_list(response);
        let content_type = response_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        Some(Self {
            file: file.to_owned(),
            method: request.get("method")?.as_str()?.to_owned(),
            url: request.get("url")?.as_str()?.replace(":443/", "/"),
            headers: header_list(Some(request)),
            body: request
                .pointer("/body/text")
                .and_then(Value::as_str)
                .map(str::to_owned),
            served_locally: value
                .get("served_locally")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            status: response
                .and_then(|r| r.get("status"))
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok()),
            content_type,
            response: response
                .and_then(|r| r.pointer("/body/text"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    fn host(&self) -> &str {
        self.url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(['/', ':']).next())
            .unwrap_or_default()
    }

    fn path(&self) -> &str {
        let rest = self.url.split("://").nth(1).unwrap_or_default();
        let path = rest.find('/').map_or("/", |i| &rest[i..]);
        path.split('?').next().unwrap_or(path)
    }

    /// Request identity used for de-duplication and online lookups.
    fn key(&self) -> String {
        format!(
            "{} {} {}",
            self.method,
            self.url,
            self.body.as_deref().unwrap_or("")
        )
    }
}

/// The storefront in Cider's captured catalog requests
/// (`/v1/catalog/<storefront>/…`), so generated requests use the same one.
/// Falls back to `us` when no catalog request was captured.
fn storefront(captures: &[Captured]) -> String {
    captures
        .iter()
        .find_map(|c| {
            let rest = c.path().strip_prefix("/v1/catalog/")?;
            let id = rest.split('/').next()?;
            (id.len() == 2 && id.bytes().all(|b| b.is_ascii_lowercase())).then_some(id)
        })
        .unwrap_or("us")
        .to_owned()
}

fn load_captures(dir: &Path) -> anyhow::Result<Vec<Captured>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            Path::new(n)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        })
        .collect();
    names.sort();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let Ok(text) = std::fs::read_to_string(dir.join(&name)) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(captured) = Captured::parse(&name, &value) {
            out.push(captured);
        }
    }
    Ok(out)
}

// ----- Patterns --------------------------------------------------------------

fn is_id_segment(segment: &str) -> bool {
    (segment.len() >= 3 && segment.bytes().all(|b| b.is_ascii_digit()))
        || ["i.", "l.", "p.", "pl.", "a.", "r.", "sp.", "ra.", "mv."]
            .iter()
            .any(|p| segment.starts_with(p) && segment.len() > p.len() + 2)
}

/// Request pattern: method + host class + templated path.
fn pattern(method: &str, host: &str, path: &str) -> String {
    if host.ends_with("mzstatic.com") {
        return format!("{method} artwork (mzstatic)");
    }
    if host.ends_with("blobstore.apple.com") {
        return format!("{method} artwork (blobstore)");
    }
    if path.starts_with("/__cider-offline/media/") {
        return format!("{method} {host} /__cider-offline/media/{{id}}");
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut out = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        let templated = if is_id_segment(part) {
            "{id}"
        } else if i == 2
            && parts.first() == Some(&"v1")
            && matches!(parts.get(1), Some(&"catalog" | &"social" | &"editorial"))
        {
            "{sf}"
        } else {
            part
        };
        out.push(templated);
    }
    format!("{method} {host} /{}", out.join("/"))
}

// ----- Report ------------------------------------------------------------------

#[derive(Debug, Default)]
struct PatternStats {
    requests: usize,
    passed: usize,
    notes: BTreeMap<String, usize>,
    failures: Vec<String>,
}

#[derive(Default)]
struct Report {
    patterns: BTreeMap<String, PatternStats>,
    content: Vec<(String, bool, String)>,
    verbose: bool,
}

impl Report {
    fn line(text: &str) {
        // Best effort: a closed stdout must not abort the check.
        let _ = writeln!(std::io::stdout().lock(), "{text}");
    }

    fn record(&mut self, pattern: &str, outcome: Outcome) {
        let stats = self.patterns.entry(pattern.to_owned()).or_default();
        stats.requests += 1;
        for note in outcome.notes {
            *stats.notes.entry(note).or_default() += 1;
        }
        if outcome.failures.is_empty() {
            stats.passed += 1;
        } else {
            for failure in outcome.failures {
                if stats.failures.len() < 6 || self.verbose {
                    stats.failures.push(failure);
                }
            }
        }
    }

    fn content(&mut self, name: &str, ok: bool, detail: String) {
        self.content.push((name.to_owned(), ok, detail));
    }

    fn print(&self) -> bool {
        Self::line("");
        Self::line("== Per-pattern results (replayed captured requests + variants) ==");
        let mut all_ok = true;
        let (mut total, mut passed) = (0, 0);
        for (pattern, stats) in &self.patterns {
            total += stats.requests;
            passed += stats.passed;
            let ok = stats.passed == stats.requests;
            all_ok &= ok;
            Self::line(&format!(
                "{} {pattern}: {}/{} passed",
                if ok { "PASS" } else { "FAIL" },
                stats.passed,
                stats.requests
            ));
            for (note, count) in &stats.notes {
                Self::line(&format!("       note ({count}x): {note}"));
            }
            for failure in &stats.failures {
                Self::line(&format!("       FAIL: {failure}"));
            }
        }
        Self::line("");
        Self::line("== Content checks ==");
        let mut content_ok = 0;
        for (name, ok, detail) in &self.content {
            all_ok &= ok;
            content_ok += usize::from(*ok);
            Self::line(&format!(
                "{} {name}: {detail}",
                if *ok { "PASS" } else { "FAIL" }
            ));
        }
        Self::line("");
        Self::line(&format!(
            "SUMMARY: {} patterns, {total} requests, {passed} passed, {} failed; content checks {content_ok}/{} passed",
            self.patterns.len(),
            total - passed,
            self.content.len()
        ));
        Self::line(if all_ok {
            "RESULT: PASS"
        } else {
            "RESULT: FAIL"
        });
        all_ok
    }
}

/// Result of checking one response.
#[derive(Debug, Default)]
struct Outcome {
    failures: Vec<String>,
    notes: Vec<String>,
}

impl Outcome {
    fn fail(&mut self, text: impl Into<String>) {
        self.failures.push(text.into());
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

// ----- Local oracle ------------------------------------------------------------

/// What the store holds, read independently of the responder.
#[derive(Debug, Default)]
struct Oracle {
    synced: Vec<SyncedCollection>,
    /// Every id (row id, catalog id) of a stored track.
    track_ids: HashSet<String>,
    /// Row id → catalog id.
    catalog_of: HashMap<String, String>,
    /// Ids (row or catalog) of tracks with local audio.
    downloaded: HashSet<String>,
    /// Collection ids, playlist global ids and catalog album ids.
    collection_ids: HashSet<String>,
    album_ids: HashSet<String>,
    titles: HashMap<String, (String, String)>,
    /// Apple's library Songs tab as synced (Apple's `dateAdded` order).
    songs: Vec<String>,
    /// Every saved library song ("all" Songs page): `i.…` library songs,
    /// songs proven saved, library album members and the Songs tab.
    all_songs: HashSet<String>,
}

impl Oracle {
    async fn load(storage: &Storage) -> anyhow::Result<Self> {
        let mut oracle = Self {
            synced: storage.synced_library().await?,
            songs: storage.synced_library_songs().await?,
            ..Self::default()
        };
        oracle.all_songs.extend(oracle.songs.iter().cloned());
        for synced in &oracle.synced {
            if synced.collection.kind == CollectionKind::Album {
                oracle.all_songs.extend(synced.track_ids.iter().cloned());
            }
        }
        for track in storage.list_resources().await? {
            let metadata: Option<Value> = track
                .metadata
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok());
            if !track.is_custom
                && metadata.as_ref().is_some_and(|m| m.get("id").is_some())
                && (track.in_library || track.id.starts_with("i."))
            {
                oracle.all_songs.insert(track.id.clone());
            }
            if let Some(url) = metadata.as_ref().and_then(|m| {
                m.pointer("/relationships/catalog/data/0/attributes/url")
                    .or_else(|| m.pointer("/attributes/url"))
                    .and_then(Value::as_str)
            }) && let Some(album) = url.split('?').next().and_then(|p| p.rsplit('/').next())
            {
                oracle.album_ids.insert(album.to_owned());
            }
            let downloaded = track.audio.iter().any(|(_, path)| path.is_file());
            for id in std::iter::once(track.id.clone()).chain(track.catalog_id.clone()) {
                if downloaded {
                    oracle.downloaded.insert(id.clone());
                }
                oracle.track_ids.insert(id);
            }
            if let Some(catalog) = &track.catalog_id {
                oracle.catalog_of.insert(track.id.clone(), catalog.clone());
            }
            oracle.titles.insert(
                track.id.clone(),
                (track.title.clone(), track.artist.clone()),
            );
        }
        for synced in &oracle.synced {
            oracle.collection_ids.insert(synced.collection.id.clone());
            if let Some(global) = synced
                .collection
                .metadata
                .as_deref()
                .and_then(|m| serde_json::from_str::<Value>(m).ok())
                .and_then(|m| {
                    m.pointer("/attributes/playParams/globalId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            {
                oracle.collection_ids.insert(global);
            }
        }
        Ok(oracle)
    }

    fn is_downloaded(&self, id: &str) -> bool {
        self.downloaded.contains(id)
    }
}

/// What a request may answer: whether the resource it names exists locally
/// (`None`: not an id lookup), and whether a local answer must hold data.
fn expectation(oracle: &Oracle, path: &str) -> (Option<bool>, bool) {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Sub-resources (`songs/{id}/audio-analysis` …): the id decides 200 vs
    // 404, but an empty list is a valid local answer.
    if let ["v1", "catalog", _, "songs" | "music-videos", id, rel] = parts.as_slice()
        && *rel != "lyrics"
        && *rel != "syllable-lyrics"
    {
        return (Some(oracle.track_ids.contains(*id)), false);
    }
    let local = local_resource(oracle, &parts);
    (local, local == Some(true))
}

fn local_resource(oracle: &Oracle, parts: &[&str]) -> Option<bool> {
    match parts {
        ["v1", "catalog", _, "songs" | "music-videos", id]
        | ["v1", "me", "library", "songs" | "music-videos", id] => {
            Some(oracle.track_ids.contains(*id))
        }
        ["v1", "catalog", _, "albums", id] | ["v1", "catalog", _, "albums", id, "tracks"] => {
            Some(oracle.album_ids.contains(*id) || oracle.collection_ids.contains(*id))
        }
        ["v1", "catalog", _, "playlists", id]
        | ["v1", "catalog", _, "playlists", id, "tracks"]
        | ["v1", "me", "library", "playlists" | "albums", id] => {
            Some(oracle.collection_ids.contains(*id))
        }
        ["v1", "me", "library", "playlists" | "albums", id, "tracks"] => {
            // A synced collection without tracks answers an empty page.
            oracle
                .synced
                .iter()
                .find(|s| s.collection.id == *id)
                .map(|s| !s.track_ids.is_empty())
                .or(Some(false))
        }
        // The library listings: non-empty whenever the synced library is.
        [
            "v1",
            "me",
            "library",
            "playlist-folders",
            "p.playlistsroot",
            "children",
        ]
        | ["v1", "me", "library", "albums" | "recently-added"] => Some(!oracle.synced.is_empty()),
        [
            "v1",
            "catalog",
            _,
            "artists" | "curators" | "stations" | "record-labels",
            ..,
        ] => Some(false),
        _ => None,
    }
}

// ----- Checks ------------------------------------------------------------------

/// A response as seen by the client.
struct Seen {
    status: u16,
    content_type: String,
    bytes: Vec<u8>,
}

impl Seen {
    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.bytes).ok()
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Valid JSON:API document: an object with `data`, `results` or `errors`,
/// whose `data` resources carry `id` and `type`.
fn check_jsonapi(value: &Value, outcome: &mut Outcome) {
    let Some(object) = value.as_object() else {
        outcome.fail("top level is not an object");
        return;
    };
    if !["data", "results", "errors"]
        .iter()
        .any(|k| object.contains_key(*k))
        && !object.is_empty()
    {
        outcome.note("no data/results/errors member (non JSON:API endpoint)");
    }
    if let Some(data) = object.get("data").and_then(Value::as_array) {
        for resource in data {
            if resource.get("id").is_none() || resource.get("type").is_none() {
                outcome.fail("a data resource lacks id/type");
                break;
            }
        }
    }
}

/// Compares our resource with the online one (keys and value types).
fn compare_resource(ours: &Value, online: &Value, at: &str, outcome: &mut Outcome) {
    if ours.get("type") != online.get("type") {
        outcome.fail(format!(
            "{at}: type {} (online {})",
            ours.get("type").unwrap_or(&Value::Null),
            online.get("type").unwrap_or(&Value::Null)
        ));
        return;
    }
    for key in ["id", "type", "href", "attributes"] {
        if online.get(key).is_some() && ours.get(key).is_none() {
            outcome.fail(format!("{at}: missing {key}"));
        }
    }
    if let (Some(ours), Some(online)) = (
        ours.get("attributes").and_then(Value::as_object),
        online.get("attributes").and_then(Value::as_object),
    ) {
        for (key, value) in online {
            match ours.get(key) {
                None if key == "playParams" => {}
                None if TOLERATED_ATTRIBUTES.contains(&key.as_str()) => {
                    outcome.note(format!("attribute {key} not available offline"));
                }
                None => outcome.fail(format!("{at}: missing attribute {key}")),
                Some(mine) if type_name(mine) != type_name(value) && !value.is_null() => {
                    outcome.fail(format!(
                        "{at}: attribute {key} is {} (online {})",
                        type_name(mine),
                        type_name(value)
                    ));
                }
                Some(_) => {}
            }
        }
    }
    if let Some(online_rel) = online.get("relationships").and_then(Value::as_object) {
        let ours_rel = ours.get("relationships").and_then(Value::as_object);
        for (name, relationship) in online_rel {
            match ours_rel.and_then(|r| r.get(name)) {
                None if TOLERATED_RELATIONSHIPS.contains(&name.as_str()) => {
                    outcome.note(format!("relationship {name} not available offline"));
                }
                None => outcome.fail(format!("{at}: missing relationship {name}")),
                Some(mine) => {
                    if relationship.get("next").is_some()
                        && mine.get("next").is_none()
                        && mine.pointer("/meta/total").and_then(Value::as_u64)
                            > mine
                                .get("data")
                                .and_then(Value::as_array)
                                .map(|d| d.len() as u64)
                    {
                        outcome.fail(format!("{at}: relationship {name} lacks next"));
                    }
                    let same = relationship.pointer("/data/0/id").and_then(|id| {
                        mine.get("data")
                            .and_then(Value::as_array)
                            .and_then(|d| d.iter().find(|r| r.get("id") == Some(id)))
                    });
                    if let (Some(a), Some(b)) = (
                        same.or_else(|| mine.pointer("/data/0")),
                        relationship.pointer("/data/0"),
                    ) && a.get("attributes").is_some()
                        && b.get("attributes").is_some()
                    {
                        compare_resource(a, b, &format!("{at}.{name}[0]"), outcome);
                    }
                }
            }
        }
    }
}

/// Compares our document with the captured online one.
fn compare_documents(ours: &Value, online: &Value, outcome: &mut Outcome) {
    let (Some(ours_obj), Some(online_obj)) = (ours.as_object(), online.as_object()) else {
        return;
    };
    for key in online_obj.keys() {
        if !ours_obj.contains_key(key) {
            if key == "next" {
                // Only a failure when ours has more items than it shows.
                let total = ours.pointer("/meta/total").and_then(Value::as_u64);
                let shown = ours
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|d| d.len() as u64);
                if total.is_some_and(|t| Some(t) > shown) {
                    outcome.fail("missing next link");
                } else {
                    outcome.note("online had a next page; local list is complete");
                }
            } else if key == "meta" || key == "views" {
                outcome.note(format!("top-level {key} not available offline"));
            } else {
                outcome.fail(format!("missing top-level {key}"));
            }
        }
    }
    if let (Some(mine), Some(theirs)) = (ours.pointer("/data/0"), online.pointer("/data/0")) {
        // Compare the same item when both lists hold it (optional attributes
        // such as `contentRating` differ between items).
        let mine = theirs
            .get("id")
            .and_then(|id| {
                ours.get("data")
                    .and_then(Value::as_array)
                    .and_then(|d| d.iter().find(|r| r.get("id") == Some(id)))
            })
            .unwrap_or(mine);
        compare_resource(mine, theirs, "data[0]", outcome);
    } else if online.pointer("/data/0").is_some()
        && ours
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        outcome.note("online had data; nothing local");
    }
    if let (Some(mine), Some(theirs)) = (
        ours.get("results").and_then(Value::as_object),
        online.get("results").and_then(Value::as_object),
    ) {
        for (group, value) in theirs {
            if value.get("data").is_none() {
                continue;
            }
            if let Some(local) = mine.get(group) {
                for key in value
                    .as_object()
                    .map(|o| o.keys().collect::<Vec<_>>())
                    .unwrap_or_default()
                {
                    if local.get(key).is_none() && key != "next" {
                        outcome.fail(format!("results.{group} lacks {key}"));
                    }
                }
                if let (Some(a), Some(b)) = (local.pointer("/data/0"), value.pointer("/data/0"))
                    && a.get("type") == b.get("type")
                {
                    compare_resource(a, b, &format!("results.{group}[0]"), outcome);
                }
            }
        }
    }
    if let (Some(mine), Some(theirs)) = (ours.get("resources"), online.get("resources"))
        && let (Some(mine), Some(theirs)) = (mine.as_object(), theirs.as_object())
    {
        for (ty, bucket) in theirs {
            if let (Some(local), Some(first)) = (
                mine.get(ty).and_then(Value::as_object),
                bucket.as_object().and_then(|b| b.values().next()),
            ) && let Some(ours_first) = local.values().next()
            {
                compare_resource(ours_first, first, &format!("resources.{ty}"), outcome);
            }
        }
    }
}

// ----- Client ------------------------------------------------------------------

struct Client {
    http: reqwest::Client,
    /// Storefront of the catalog requests this client builds.
    storefront: String,
}

impl Client {
    fn new(proxy: std::net::SocketAddr, ca_pem: &str, storefront: &str) -> anyhow::Result<Self> {
        Self::with_headers(proxy, ca_pem, storefront, reqwest::header::HeaderMap::new())
    }

    /// A client that sends `headers` (e.g. Cider's auth headers) with every
    /// request.
    fn with_headers(
        proxy: std::net::SocketAddr,
        ca_pem: &str,
        storefront: &str,
        headers: reqwest::header::HeaderMap,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{proxy}"))?)
            .tls_certs_only([reqwest::Certificate::from_pem(ca_pem.as_bytes())?])
            .default_headers(headers)
            .build()?;
        Ok(Self {
            http,
            storefront: storefront.to_owned(),
        })
    }

    /// Catalog base URL for the client's storefront, without a trailing `/`.
    fn catalog(&self) -> String {
        format!("{API}/v1/catalog/{}", self.storefront)
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> anyhow::Result<Seen> {
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut request = self.http.request(method, url);
        for (name, value) in headers {
            if DROPPED_HEADERS.iter().any(|d| d.eq_ignore_ascii_case(name)) || name.starts_with(':')
            {
                continue;
            }
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            request = request.body(body.to_owned());
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await?.to_vec();
        Ok(Seen {
            status,
            content_type,
            bytes,
        })
    }

    async fn get(&self, url: &str) -> anyhow::Result<Seen> {
        self.send("GET", url, &[], None).await
    }

    async fn get_json(&self, url: &str) -> anyhow::Result<(u16, Value)> {
        let seen = self.get(url).await?;
        Ok((seen.status, seen.json().unwrap_or(Value::Null)))
    }
}

/// Checks one response for `captured` (a replayed or variant request).
#[expect(clippy::too_many_lines, reason = "one rule per host / endpoint class")]
fn check_response(
    captured: &Captured,
    seen: &Seen,
    oracle: &Oracle,
    online: Option<&Value>,
) -> Outcome {
    let mut outcome = Outcome::default();
    let host = captured.host();
    let path = captured.path();
    let status = seen.status;

    if host.ends_with("mzstatic.com") || host.ends_with("blobstore.apple.com") {
        match status {
            200 => {
                if !seen.content_type.starts_with("image/") {
                    outcome.fail(format!("content-type {}", seen.content_type));
                }
                if captured.method == "GET" && image::load_from_memory(&seen.bytes).is_err() {
                    outcome.fail("body is not a decodable image");
                }
            }
            404 => outcome.note("image never cached (not referenced by the library)"),
            other => outcome.fail(format!("status {other}")),
        }
        return outcome;
    }
    if host == "p3-buy.itunes.apple.com" {
        if status == 503 {
            outcome.note("account info: intentionally unavailable offline (personal data)");
        } else if !(200..300).contains(&status) {
            outcome.fail(format!("status {status}"));
        }
        return outcome;
    }
    if host.ends_with("audio-ssl.itunes.apple.com") || host.starts_with("aod") {
        outcome.note("streamed / preview audio: unavailable offline (local media is used instead)");
        return outcome;
    }
    if path.starts_with("/__cider-offline/media/") {
        let id = path.rsplit('/').next().unwrap_or_default();
        if oracle.is_downloaded(id) {
            if !(status == 200 || status == 206) || seen.bytes.is_empty() {
                outcome.fail(format!("downloaded track media status {status}"));
            }
        } else if status == 404 {
            outcome.note("media of a track that is no longer downloaded");
        } else if status != 200 && status != 206 {
            outcome.fail(format!("status {status}"));
        }
        return outcome;
    }
    if host == "play.itunes.apple.com" {
        if path.ends_with("/webPlayback") {
            let Some(body) = seen.json() else {
                outcome.fail("webPlayback answer is not JSON");
                return outcome;
            };
            let requested: Option<String> = captured
                .body
                .as_deref()
                .and_then(|b| serde_json::from_str::<Value>(b).ok())
                .and_then(|b| {
                    [
                        "salableAdamId",
                        "universalLibraryId",
                        "subscriptionAdamId",
                        "adamId",
                    ]
                    .iter()
                    .find_map(|k| b.get(*k).and_then(Value::as_str).map(str::to_owned))
                });
            let downloaded = requested
                .as_deref()
                .is_some_and(|id| oracle.is_downloaded(id));
            let local_url = body
                .pointer("/songList/0/assets/0/URL")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("/__cider-offline/media/"));
            if downloaded && !local_url {
                outcome.fail("downloaded track did not get a local media URL");
            } else if !downloaded && body.get("failureType").is_none() && !local_url {
                outcome.fail("non-downloaded track got neither media nor a failure");
            }
            if !downloaded {
                outcome.note("track not downloaded: 'not available offline' answer");
            }
        } else if status == 503 {
            outcome.note("playback service call unavailable offline (license/cert)");
        } else if !(200..300).contains(&status) {
            outcome.fail(format!("status {status}"));
        }
        return outcome;
    }
    if host == "universal-activity-service.itunes.apple.com" {
        if status != 200 {
            outcome.fail(format!("status {status}"));
        }
        return outcome;
    }

    // amp-api
    let (local, must_have_data) = expectation(oracle, path);
    let paged_past_start = reqwest::Url::parse(&captured.url).is_ok_and(|u| {
        u.query_pairs()
            .any(|(k, v)| k == "offset" && v.parse::<usize>().is_ok_and(|o| o > 0))
    });
    let lyrics = path.ends_with("/lyrics") || path.ends_with("/syllable-lyrics");
    if captured.method == "PUT" || captured.method == "DELETE" || captured.method == "PATCH" {
        if status == 503 {
            outcome.note("library edit: intentionally unavailable offline");
        } else if !(200..300).contains(&status) {
            outcome.fail(format!("status {status}"));
        }
        return outcome;
    }
    match status {
        200..=299 => {}
        404 if lyrics => {
            outcome.note("no local lyrics for this song");
            return outcome;
        }
        404 if local == Some(false) => {
            outcome.note("resource not in the local library (404)");
            return outcome;
        }
        other => {
            outcome.fail(format!(
                "status {other}{}",
                if local == Some(true) {
                    " for a LOCAL resource"
                } else {
                    ""
                }
            ));
            return outcome;
        }
    }
    if lyrics {
        let document = seen.json();
        lyrics::check_lyrics_body(&seen.content_type, document.as_ref(), path, &mut outcome);
        return outcome;
    }
    if !seen.content_type.starts_with("application/json") {
        outcome.fail(format!("content-type {:?}", seen.content_type));
        return outcome;
    }
    let Some(document) = seen.json() else {
        outcome.fail("invalid JSON");
        return outcome;
    };
    check_jsonapi(&document, &mut outcome);
    if must_have_data
        && !paged_past_start
        && document
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        outcome.fail("empty data for a LOCAL resource");
    }
    if let Some(online) = online {
        compare_documents(&document, online, &mut outcome);
    } else {
        outcome.note("no online capture to compare with");
    }
    outcome
}

/// The online reference for `captured`: the same request answered upstream,
/// or else another upstream answer of the same pattern and query keys.
fn online_reference<'a>(
    captured: &Captured,
    exact: &'a HashMap<String, Value>,
    by_shape: &'a HashMap<String, Value>,
) -> Option<&'a Value> {
    exact
        .get(&captured.key())
        .or_else(|| by_shape.get(&shape_key(captured)))
}

fn query_keys(url: &str) -> String {
    let mut keys: Vec<String> = reqwest::Url::parse(url)
        .map(|u| u.query_pairs().map(|(k, _)| k.into_owned()).collect())
        .unwrap_or_default();
    keys.sort();
    keys.dedup();
    keys.join(",")
}

fn shape_key(captured: &Captured) -> String {
    format!(
        "{} {}",
        pattern(&captured.method, captured.host(), captured.path()),
        query_keys(&captured.url)
    )
}

// ----- Setup -------------------------------------------------------------------

/// Removes the temporary directory when the check ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Mirrors `from` into `to` (hard links, falling back to copies).
fn mirror_dir(from: &Path, to: &Path) -> anyhow::Result<usize> {
    std::fs::create_dir_all(to)?;
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(0);
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let target = to.join(entry.file_name());
        if std::fs::hard_link(&path, &target).is_err() {
            std::fs::copy(&path, &target).with_context(|| format!("copying {}", path.display()))?;
        }
        count += 1;
    }
    Ok(count)
}

async fn prepare(real: &AppPaths, dir: &Path) -> anyhow::Result<AppPaths> {
    let paths = AppPaths::under(dir);
    paths.ensure_dirs()?;
    let source = Storage::open_async(real.database_file())
        .await
        .context("opening the real store")?;
    source
        .backup_into(paths.database_file())
        .await
        .context("copying the store")?;
    drop(source);
    let artwork = mirror_dir(&real.artwork_dir(), &paths.artwork_dir())?;
    let lyrics = mirror_dir(&real.lyrics_dir(), &paths.lyrics_dir())?;
    let media = mirror_dir(&real.media_dir(), &paths.media_dir())?;
    Report::line(&format!(
        "temporary data dir {} (store copied; {artwork} artwork, {lyrics} lyrics, {media} media files linked)",
        paths.data_dir().display()
    ));
    Ok(paths)
}

async fn start_offline(paths: &AppPaths) -> anyhow::Result<ProxyHandle> {
    let mut config = Config {
        mode: Mode::Active,
        capture: false,
        connectivity: ConnectivitySettings {
            enabled: false,
            force_offline: true,
            ..ConnectivitySettings::default()
        },
        ..Config::default()
    };
    config.proxy.port = 0;
    let handle = ProxyHandle::builder(config)
        .paths(paths.clone())
        .start()
        .await
        .context("starting the proxy")?;
    Ok(handle)
}

// ----- Main flow ---------------------------------------------------------------

#[expect(clippy::too_many_lines, reason = "linear setup, replay and report")]
async fn check(options: &Options) -> anyhow::Result<bool> {
    let real = crate::startup::discover_paths()?;
    let captures_dir = options
        .captures
        .clone()
        .unwrap_or_else(|| real.captures_dir());
    let started = Instant::now();
    let captures = load_captures(&captures_dir)?;
    Report::line(&format!(
        "loaded {} captures from {}",
        captures.len(),
        captures_dir.display()
    ));

    // Online references: upstream answers (not served locally) with JSON bodies.
    let mut exact: HashMap<String, Value> = HashMap::new();
    let mut by_shape: HashMap<String, Value> = HashMap::new();
    for c in captures
        .iter()
        .filter(|c| !c.served_locally && c.status == Some(200))
    {
        if let Some(value) = c
            .response
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
        {
            by_shape
                .entry(shape_key(c))
                .or_insert_with(|| value.clone());
            exact.insert(c.key(), value);
        }
    }

    let dir = std::env::temp_dir().join(format!("cider-offline-check-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("clearing the temporary dir")?;
    }
    let _guard = TempDir(dir.clone());
    let paths = prepare(&real, &dir).await?;
    let handle = start_offline(&paths).await?;
    // The Songs tab comes only from the API sync: sync the copy from the
    // live Apple Music API first (stored tokens; nothing is printed).
    let synced = handle.library_sync().sync_now().await;
    let linked = handle.library().link_existing_media().await;
    let oracle = Oracle::load(handle.storage()).await?;
    Report::line(&format!(
        "proxy forced offline on {} | store: {} synced collections, {} track ids, {} downloaded ids ({linked} re-linked from the media dir)",
        handle.local_addr(),
        oracle.synced.len(),
        oracle.track_ids.len(),
        oracle.downloaded.len()
    ));
    let client = Client::new(
        handle.local_addr(),
        handle.certificate_authority().cert_pem(),
        &storefront(&captures),
    )?;
    let mut report = Report {
        verbose: options.verbose,
        ..Report::default()
    };
    report.content(
        "library synced from the live Apple Music API into the copy",
        synced.is_ok(),
        match &synced {
            Ok(snapshot) => format!(
                "{} collections, {} tracks, Songs tab {} songs",
                snapshot.collections.len(),
                snapshot.tracks.len(),
                snapshot.songs.len()
            ),
            Err(error) => format!("{error}"),
        },
    );

    if options.baseline {
        baseline(&captures, &oracle, &exact, &by_shape, &mut report);
    } else {
        let mut seen_keys = HashSet::new();
        let mut documents: Vec<Value> = Vec::new();
        for captured in &captures {
            if captured.method == "OPTIONS"
                || !REPLAYED_HOSTS.contains(&captured.host())
                || !seen_keys.insert(captured.key())
            {
                continue;
            }
            let pattern = pattern(&captured.method, captured.host(), captured.path());
            let outcome = match client
                .send(
                    &captured.method,
                    &captured.url,
                    &captured.headers,
                    captured.body.as_deref(),
                )
                .await
            {
                Ok(seen) => {
                    if let Some(document) = seen.json() {
                        documents.push(document);
                    }
                    check_response(
                        captured,
                        &seen,
                        &oracle,
                        online_reference(captured, &exact, &by_shape),
                    )
                }
                Err(error) => {
                    let mut outcome = Outcome::default();
                    outcome.fail(format!("request failed: {error:#}"));
                    outcome
                }
            };
            if options.verbose && !outcome.failures.is_empty() {
                Report::line(&format!(
                    "  {} {} -> {:?}",
                    captured.file, captured.url, outcome.failures
                ));
            }
            report.record(&pattern, outcome);
        }
        content_checks(&client, &oracle, &captures, &mut report, &mut documents).await?;
        songs_page_checks(&handle, &client, &oracle, &captures, &mut report).await?;
        variant_checks(&client, &oracle, &mut report).await?;
        artwork_checks(&client, &documents, &mut report).await;
        lyrics::lyrics_checks(&handle, &client, &mut report).await?;
        online_checks(&handle, &oracle, &captures, &mut report).await?;
    }

    let ok = report.print();
    Report::line(&format!(
        "finished in {:.1}s",
        started.elapsed().as_secs_f32()
    ));
    handle.shutdown().await.ok();
    Ok(ok)
}

/// Evaluates what the proxy answered Cider in the captured offline sessions.
fn baseline(
    captures: &[Captured],
    oracle: &Oracle,
    exact: &HashMap<String, Value>,
    by_shape: &HashMap<String, Value>,
    report: &mut Report,
) {
    Report::line(
        "BASELINE: evaluating responses the proxy served Cider in the captured offline sessions",
    );
    let mut seen_keys = HashSet::new();
    for captured in captures.iter().filter(|c| c.served_locally) {
        if captured.method == "OPTIONS" || !seen_keys.insert(captured.key()) {
            continue;
        }
        let seen = Seen {
            status: captured.status.unwrap_or(0),
            content_type: captured.content_type.clone().unwrap_or_default(),
            // Binary bodies (media, images) are not captured: stand in for them.
            bytes: captured.response.clone().map_or_else(
                || b"(binary body not captured)".to_vec(),
                String::into_bytes,
            ),
        };
        let mut outcome = check_response(
            captured,
            &seen,
            oracle,
            online_reference(captured, exact, by_shape),
        );
        // Image bodies are not captured; judge artwork by status only.
        if captured.host().ends_with("mzstatic.com") && seen.status == 200 {
            outcome.failures.retain(|f| !f.contains("decodable"));
        }
        let pattern = pattern(&captured.method, captured.host(), captured.path());
        report.record(&pattern, outcome);
    }
    // Content view of the baseline: library listings as served.
    for captured in captures.iter().filter(|c| c.served_locally) {
        let path = captured.path();
        if path.ends_with("/playlist-folders/p.playlistsroot/children")
            && let Some(document) = captured
                .response
                .as_deref()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
        {
            let n = document["data"].as_array().map_or(0, Vec::len);
            let expected = oracle
                .synced
                .iter()
                .filter(|s| s.collection.kind == CollectionKind::Playlist)
                .count();
            report.content(
                "baseline: sidebar playlists served offline",
                n == expected,
                format!("{n} of {expected} ({})", captured.file),
            );
            break;
        }
    }
}

fn ids_of(document: &Value) -> Vec<String> {
    document
        .get("data")
        .and_then(Value::as_array)
        .map(|d| {
            d.iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn offset_of(next: &str) -> Option<usize> {
    next.split(['?', '&'])
        .find_map(|p| p.strip_prefix("offset="))
        .and_then(|o| o.parse().ok())
}

/// The query Cider used for `pattern`, from the captures (first match).
fn captured_query(captures: &[Captured], pattern_text: &str, needle: &str) -> Option<String> {
    captures
        .iter()
        .filter(|c| c.method == "GET")
        .filter(|c| pattern(&c.method, c.host(), c.path()) == pattern_text)
        .find(|c| c.url.contains(needle))
        .and_then(|c| c.url.split_once('?').map(|(_, q)| q.to_owned()))
}

/// Removes `offset=` from a query string.
fn without_offset(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.starts_with("offset="))
        .collect::<Vec<_>>()
        .join("&")
}

const API: &str = "https://amp-api.music.apple.com";

/// Pages through `path` the way Cider does (same query, `offset` from `next`).
async fn page_all(
    client: &Client,
    path: &str,
    query: &str,
    documents: &mut Vec<Value>,
) -> anyhow::Result<(Vec<String>, Option<u64>, Vec<String>)> {
    let base = without_offset(query);
    let mut ids = Vec::new();
    let mut problems = Vec::new();
    let mut total = None;
    let mut offset = 0;
    for _ in 0..100 {
        let url = format!("{API}{path}?offset={offset}&{base}");
        let (status, document) = client.get_json(&url).await?;
        if status != 200 {
            problems.push(format!("{url} -> {status}"));
            break;
        }
        total = total.or_else(|| document.pointer("/meta/total").and_then(Value::as_u64));
        ids.extend(ids_of(&document));
        let next = document
            .get("next")
            .and_then(Value::as_str)
            .map(str::to_owned);
        documents.push(document);
        let Some(next) = next else {
            break;
        };
        if !next.starts_with(path) {
            problems.push(format!("next link {next} does not continue {path}"));
            break;
        }
        match offset_of(&next) {
            Some(n) if n > offset => offset = n,
            _ => {
                problems.push(format!("next link {next} does not advance"));
                break;
            }
        }
    }
    Ok((ids, total, problems))
}

#[expect(
    clippy::too_many_lines,
    reason = "one linear list of content assertions"
)]
async fn content_checks(
    client: &Client,
    oracle: &Oracle,
    captures: &[Captured],
    report: &mut Report,
    documents: &mut Vec<Value>,
) -> anyhow::Result<()> {
    let playlists: Vec<&SyncedCollection> = oracle
        .synced
        .iter()
        .filter(|s| s.collection.kind == CollectionKind::Playlist)
        .collect();
    let albums: Vec<&SyncedCollection> = oracle
        .synced
        .iter()
        .filter(|s| s.collection.kind == CollectionKind::Album)
        .collect();

    // Sidebar / library playlists, exactly as Cider pages them.
    let folder_query = captured_query(
        captures,
        "GET amp-api.music.apple.com /v1/me/library/playlist-folders/{id}/children",
        "include=catalog",
    )
    .unwrap_or_else(|| "l=en-GB&platform=web&art%5Burl%5D=f&limit=100&include=catalog".to_owned());
    let (ids, total, problems) = page_all(
        client,
        "/v1/me/library/playlist-folders/p.playlistsroot/children",
        &folder_query,
        documents,
    )
    .await?;
    let expected: Vec<String> = playlists.iter().map(|s| s.collection.id.clone()).collect();
    report.content(
        "library playlists (playlist-folders root)",
        ids == expected && total == Some(expected.len() as u64) && problems.is_empty(),
        format!(
            "{} of {} in synced order, meta.total {total:?} {problems:?}",
            ids.len(),
            expected.len()
        ),
    );
    // The same listing in small pages (a variant Cider may send).
    let (ids, _, problems) = page_all(
        client,
        "/v1/me/library/playlist-folders/p.playlistsroot/children",
        "l=en-GB&limit=5",
        documents,
    )
    .await?;
    report.content(
        "library playlists paged by 5 via next",
        ids == expected && problems.is_empty(),
        format!("{} of {} {problems:?}", ids.len(), expected.len()),
    );

    // Library albums list (Cider's query).
    let albums_query = captured_query(
        captures,
        "GET amp-api.music.apple.com /v1/me/library/albums",
        "include=catalog",
    )
    .unwrap_or_else(|| "limit=100&l=en-GB&include=catalog&sort=dateAdded".to_owned());
    let (ids, _, problems) =
        page_all(client, "/v1/me/library/albums", &albums_query, documents).await?;
    let expected_albums: HashSet<String> = albums.iter().map(|s| s.collection.id.clone()).collect();
    let got: HashSet<String> = ids.iter().cloned().collect();
    report.content(
        "library albums list",
        got == expected_albums && problems.is_empty(),
        format!("{} of {} {problems:?}", got.len(), expected_albums.len()),
    );

    // Every collection: detail (Cider's query) + all track pages via next.
    let detail_query = captured_query(
        captures,
        "GET amp-api.music.apple.com /v1/me/library/playlists/{id}",
        "include=",
    )
    .unwrap_or_else(|| "l=en-GB&include=catalog,tracks&limit%5Btracks%5D=100".to_owned());
    let tracks_query = captured_query(
        captures,
        "GET amp-api.music.apple.com /v1/me/library/playlists/{id}/tracks",
        "include=albums",
    )
    .unwrap_or_else(|| {
        "l=en-GB&platform=web&art%5Burl%5D=f&limit=100&include=albums&extend=inFavorites".to_owned()
    });
    let library_album_query =
        "l=en-GB&platform=web&art%5Burl%5D=f&include=catalog&omit%5Bresource%5D=autos";
    let (mut collections_ok, mut collection_problems) = (0, Vec::new());
    let (mut queues_ok, mut queue_problems, mut played) = (0, Vec::new(), 0);
    for synced in &oracle.synced {
        let id = &synced.collection.id;
        let kind = match synced.collection.kind {
            CollectionKind::Album => "albums",
            CollectionKind::Playlist => "playlists",
        };
        let query = if kind == "albums" {
            library_album_query
        } else {
            detail_query.as_str()
        };
        let (status, detail) = client
            .get_json(&format!("{API}/v1/me/library/{kind}/{id}?{query}"))
            .await?;
        let mut problems = Vec::new();
        if status != 200 || detail.pointer("/data/0/id").and_then(Value::as_str) != Some(id) {
            problems.push(format!("detail status {status}"));
        }
        let embedded = detail.pointer("/data/0/relationships/tracks");
        let embedded_total = embedded
            .and_then(|t| t.pointer("/meta/total"))
            .and_then(Value::as_u64);
        if embedded_total != Some(synced.track_ids.len() as u64) {
            problems.push(format!(
                "embedded tracks total {embedded_total:?} != {}",
                synced.track_ids.len()
            ));
        }
        let embedded_ids: Vec<String> = embedded.map(ids_of).unwrap_or_default();
        if !synced.track_ids.starts_with(&embedded_ids) {
            problems.push("embedded tracks are not the synced prefix".to_owned());
        }
        documents.push(detail.clone());
        let (ids, total, paging) = page_all(
            client,
            &format!("/v1/me/library/{kind}/{id}/tracks"),
            &tracks_query,
            documents,
        )
        .await?;
        problems.extend(paging);
        if ids != synced.track_ids {
            problems.push(format!(
                "tracks {} != synced {}",
                ids.len(),
                synced.track_ids.len()
            ));
        }
        if total != Some(synced.track_ids.len() as u64) {
            problems.push(format!("meta.total {total:?}"));
        }
        if problems.is_empty() {
            collections_ok += 1;
        } else {
            collection_problems.push(format!(
                "{} ({id}): {}",
                synced.collection.name,
                problems.join("; ")
            ));
        }

        // Starting playback: the queue Cider builds from the embedded / paged
        // tracks is the tracks that keep playParams; each must stream locally.
        // Every page, so the whole queue (not just the first page) is checked.
        let mut queue_items = Vec::new();
        let mut offset = 0;
        loop {
            let (_, page) = client
                .get_json(&format!(
                    "{API}/v1/me/library/{kind}/{id}/tracks?offset={offset}&{}",
                    without_offset(&tracks_query)
                ))
                .await?;
            let items = page
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() || offset > synced.track_ids.len() {
                break;
            }
            offset += items.len();
            queue_items.extend(items);
        }
        let mut queue_problem = Vec::new();
        let mut playable = Vec::new();
        for resource in &queue_items {
            let rid = resource
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let has_params = resource.pointer("/attributes/playParams").is_some();
            let downloaded = oracle.is_downloaded(rid);
            if has_params != downloaded {
                queue_problem.push(format!(
                    "{rid}: playParams {has_params} but downloaded {downloaded}"
                ));
            }
            if has_params {
                playable.push(rid.to_owned());
            }
        }
        for rid in &playable {
            let catalog = oracle
                .catalog_of
                .get(rid)
                .cloned()
                .unwrap_or_else(|| rid.clone());
            let body =
                serde_json::json!({"universalLibraryId": rid, "subscriptionAdamId": catalog})
                    .to_string();
            let seen = client
                .send(
                    "POST",
                    "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback",
                    &[("content-type".to_owned(), "application/json".to_owned())],
                    Some(&body),
                )
                .await?;
            let url = seen.json().and_then(|v| {
                v.pointer("/songList/0/assets/0/URL")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let Some(url) = url else {
                queue_problem.push(format!("{rid}: webPlayback gave no media URL"));
                continue;
            };
            let media = client
                .http
                .get(&url)
                .header("range", "bytes=0-4095")
                .send()
                .await
                .ok();
            match media {
                Some(r) if r.status().as_u16() == 206 => {
                    let ct = r
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let bytes = r.bytes().await.map_or(0, |b| b.len());
                    if bytes == 0 || !ct.starts_with("audio/") {
                        queue_problem.push(format!("{rid}: media {bytes} bytes, {ct}"));
                    } else {
                        played += 1;
                    }
                }
                Some(r) => queue_problem.push(format!("{rid}: media status {}", r.status())),
                None => queue_problem.push(format!("{rid}: media request failed")),
            }
        }
        if queue_problem.is_empty() {
            queues_ok += 1;
        } else {
            queue_problems.push(format!(
                "{}: {}",
                synced.collection.name,
                queue_problem.join("; ")
            ));
        }
    }
    report.content(
        "every collection: detail + all track pages equal synced membership",
        collection_problems.is_empty(),
        format!(
            "{collections_ok}/{} {collection_problems:?}",
            oracle.synced.len()
        ),
    );
    report.content(
        "start playback from every collection (queue = downloaded tracks, each streams locally)",
        queue_problems.is_empty(),
        format!(
            "{queues_ok}/{} collections, {played} queue items streamed (all pages) {queue_problems:?}",
            oracle.synced.len()
        ),
    );

    search_checks(client, oracle, report, documents).await?;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one linear list of search assertions"
)]
async fn search_checks(
    client: &Client,
    oracle: &Oracle,
    report: &mut Report,
    documents: &mut Vec<Value>,
) -> anyhow::Result<()> {
    // Songs: downloaded ones and a spread of the rest.
    let mut ids: Vec<&String> = oracle
        .titles
        .keys()
        .filter(|id| oracle.is_downloaded(id))
        .take(25)
        .collect();
    ids.extend(
        oracle
            .titles
            .keys()
            .filter(|id| !oracle.is_downloaded(id))
            .step_by(60),
    );
    let (mut found, mut misses) = (0, Vec::new());
    for id in &ids {
        let (title, artist) = &oracle.titles[*id];
        let term = format!("{title} {artist}");
        let url = reqwest::Url::parse_with_params(
            &format!("{}/search", client.catalog()),
            &[
                ("term", term.as_str()),
                ("types", "songs,albums,playlists"),
                ("with", "serverBubbles"),
                ("limit", "25"),
                ("l", "en-GB"),
            ],
        )?;
        let (status, document) = client.get_json(url.as_str()).await?;
        let catalog = oracle.catalog_of.get(*id).map(String::as_str);
        let hit = document
            .pointer("/results/song/data")
            .and_then(Value::as_array)
            .is_some_and(|d| {
                d.iter().any(|s| {
                    let sid = s.get("id").and_then(Value::as_str);
                    sid == Some(id.as_str()) || sid == catalog
                })
            });
        documents.push(document);
        if status == 200 && hit {
            found += 1;
        } else {
            misses.push(term);
        }
    }
    report.content(
        "catalog search finds local songs by title + artist",
        misses.is_empty(),
        format!("{found}/{} {misses:?}", ids.len()),
    );

    // Playlists / albums by name, accent-insensitively, in library and catalog search.
    let (mut ok, mut problems) = (0, Vec::new());
    for synced in &oracle.synced {
        let name = synced.collection.name.trim();
        let stripped: String = name
            .chars()
            .map(|c| match c {
                'á' => 'a',
                'č' => 'c',
                'ď' => 'd',
                'é' | 'ě' => 'e',
                'í' => 'i',
                'ň' => 'n',
                'ó' => 'o',
                'ř' => 'r',
                'š' => 's',
                'ť' => 't',
                'ú' | 'ů' => 'u',
                'ý' => 'y',
                'ž' => 'z',
                'Č' => 'C',
                'Ř' => 'R',
                'Š' => 'S',
                'Ž' => 'Z',
                other => other,
            })
            .collect();
        let ty = match synced.collection.kind {
            CollectionKind::Album => "library-albums",
            CollectionKind::Playlist => "library-playlists",
        };
        let url = reqwest::Url::parse_with_params(
            &format!("{API}/v1/me/library/search"),
            &[
                ("term", stripped.as_str()),
                ("types", "library-songs,library-albums,library-playlists"),
                ("limit", "25"),
            ],
        )?;
        let (status, document) = client.get_json(url.as_str()).await?;
        let hit = document
            .pointer(&format!("/results/{ty}/data"))
            .and_then(Value::as_array)
            .is_some_and(|d| {
                d.iter()
                    .any(|r| r.get("id").and_then(Value::as_str) == Some(&synced.collection.id))
            });
        documents.push(document);
        if status == 200 && hit {
            ok += 1;
        } else {
            problems.push(format!("{name} (searched {stripped:?})"));
        }
    }
    report.content(
        "library search finds every playlist and album by name (accent-insensitive)",
        problems.is_empty(),
        format!("{ok}/{} {problems:?}", oracle.synced.len()),
    );

    // Suggestions for a partial title.
    if let Some((title, _)) = oracle.titles.values().find(|(t, _)| t.len() > 4) {
        let partial: String = title.chars().take(4).collect();
        let url = reqwest::Url::parse_with_params(
            &format!("{}/search/suggestions", client.catalog()),
            &[
                ("term", partial.as_str()),
                ("kinds", "terms,topResults"),
                ("types", "albums,songs,playlists"),
                ("limit[results:topResults]", "10"),
            ],
        )?;
        let (status, document) = client.get_json(url.as_str()).await?;
        let top = document
            .pointer("/results/suggestions")
            .and_then(Value::as_array)
            .map_or(0, |s| {
                s.iter().filter(|x| x["kind"] == "topResults").count()
            });
        report.content(
            "search suggestions offer local top results",
            status == 200 && top > 0,
            format!("term {partial:?}: {top} top results"),
        );
    }
    Ok(())
}

/// Requests Cider plausibly sends offline that were never captured.
#[expect(clippy::too_many_lines, reason = "one linear list of variant requests")]
async fn variant_checks(
    client: &Client,
    oracle: &Oracle,
    report: &mut Report,
) -> anyhow::Result<()> {
    const SONG_QUERIES: &[&str] = &[
        "l=en-GB&platform=web&art%5Burl%5D=f&include%5Bsongs%5D=audio-analysis&fields%5Bsongs%5D=id&omit%5Bresource%5D=autos&extend=fades",
        "l=en-GB&platform=web&art%5Burl%5D=f&extend=inFavorites,artistUrl,credits&include=artists,albums&fields%5Bartists%5D=name,url,artwork&fields%5Balbums%5D=name,artwork,playParams,url,artistName",
        "l=en-GB&platform=web&art%5Burl%5D=f&fields=inFavorites,inLibrary&include=library&include%5Blibrary-songs%5D=albums&omit%5Bresource%5D=autos",
        "l=en-GB&platform=web&art%5Burl%5D=f&fields=audioLocale",
        "extend=extendedAssetUrls&omit%5Bresource%5D=autos",
    ];
    // Catalog lookups for every local song (by catalog id and by library id).
    for (row, catalog) in &oracle.catalog_of {
        for (i, query) in SONG_QUERIES.iter().enumerate() {
            let id = if i % 2 == 0 { catalog } else { row };
            let url = format!("{}/songs/{id}?{query}", client.catalog());
            let seen = client.get(&url).await?;
            let mut outcome = Outcome::default();
            let document = seen.json();
            if seen.status != 200 {
                outcome.fail(format!("{url} -> {}", seen.status));
            } else if document
                .as_ref()
                .and_then(|d| d.pointer("/data/0/id"))
                .is_none()
            {
                outcome.fail(format!("{url}: no data"));
            }
            report.record(
                "VARIANT GET /v1/catalog/{sf}/songs/{id} (every local song, Cider's queries)",
                outcome,
            );
        }
    }
    // Multi-type map lookups in batches like Cider's.
    let mut catalogs: Vec<&String> = oracle.catalog_of.values().collect();
    catalogs.sort();
    catalogs.dedup();
    for chunk in catalogs.chunks(20).take(20) {
        let ids = chunk
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let url = format!(
            "{}?l=en-GB&platform=web&art%5Burl%5D=f&fields%5Bsongs%5D=inLibrary,personalRating,inFavorites&ids%5Bsongs%5D={ids}&omit%5Bresource%5D=autos&relate=library&format%5Bresources%5D=map",
            client.catalog()
        );
        let (status, document) = client.get_json(&url).await?;
        let mut outcome = Outcome::default();
        let n = document
            .pointer("/resources/songs")
            .and_then(Value::as_object)
            .map_or(0, serde_json::Map::len);
        if status != 200 || n != chunk.len() {
            outcome.fail(format!(
                "map lookup of {} songs gave {n} (status {status})",
                chunk.len()
            ));
        }
        report.record(
            "VARIANT GET /v1/catalog/{sf}?ids[songs]=… (format[resources]=map batches)",
            outcome,
        );
    }
    // Other page sizes on every collection's tracks.
    for synced in &oracle.synced {
        let kind = match synced.collection.kind {
            CollectionKind::Album => "albums",
            CollectionKind::Playlist => "playlists",
        };
        for (offset, limit) in [(0, 25), (25, 25), (50, 10)] {
            let url = format!(
                "{API}/v1/me/library/{kind}/{}/tracks?offset={offset}&limit={limit}&l=en-US",
                synced.collection.id
            );
            let (status, document) = client.get_json(&url).await?;
            let expected: Vec<String> = synced
                .track_ids
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect();
            let mut outcome = Outcome::default();
            if status != 200 || ids_of(&document) != expected {
                outcome.fail(format!(
                    "{url}: got {} ids (status {status})",
                    ids_of(&document).len()
                ));
            }
            let more = synced.track_ids.len() > offset + limit;
            if more != document.get("next").is_some() {
                outcome.fail(format!("{url}: next link presence {}", !more));
            }
            report.record(
                "VARIANT GET /v1/me/library/{kind}/{id}/tracks (other offsets/limits)",
                outcome,
            );
        }
        // The catalog twins Cider opens from a library page.
        let (status, _) = client
            .get_json(&format!(
                "{}/{kind}/{}?l=en-GB",
                client.catalog(),
                synced.collection.id
            ))
            .await?;
        let mut outcome = Outcome::default();
        if status != 200 {
            outcome.fail(format!(
                "catalog {kind} lookup by library id {} -> {status}",
                synced.collection.id
            ));
        }
        report.record("VARIANT GET /v1/catalog/{sf}/{kind}/{library id}", outcome);
    }
    // Library songs / artists / recently added listings.
    for path in [
        "/v1/me/library/songs?offset=0&limit=100&l=en-GB&include%5Blibrary-songs%5D=albums,personalRating&sort=dateAdded&extend=inFavorites",
        "/v1/me/library/artists?offset=0&limit=100&l=en-GB&include=catalog&extend=inFavorites&sort=name",
        "/v1/me/library/recently-added?limit=25&l=en-GB&include=catalog",
        "/v1/me/recent/played?l=en-GB&limit=20",
    ] {
        let (status, document) = client.get_json(&format!("{API}{path}")).await?;
        let mut outcome = Outcome::default();
        if status != 200 || ids_of(&document).is_empty() {
            outcome.fail(format!(
                "{path} -> {status}, {} items",
                ids_of(&document).len()
            ));
        }
        report.record(
            "VARIANT GET library listings (songs/artists/recently-added/recent)",
            outcome,
        );
    }
    Ok(())
}

/// Every artwork URL referenced by the responses resolves to image bytes, at
/// several sizes and formats.
async fn artwork_checks(client: &Client, documents: &[Value], report: &mut Report) {
    /// Artwork URL -> href of the resource that references it.
    fn collect(value: &Value, owner: &str, out: &mut HashMap<String, String>) {
        match value {
            Value::Object(object) => {
                let owner = object.get("href").and_then(Value::as_str).unwrap_or(owner);
                if let Some(url) = object
                    .get("artwork")
                    .and_then(|a| a.get("url"))
                    .and_then(Value::as_str)
                {
                    out.entry(url.to_owned())
                        .or_insert_with(|| owner.to_owned());
                }
                for v in object.values() {
                    collect(v, owner, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| collect(v, owner, out)),
            _ => {}
        }
    }
    let mut owners = HashMap::new();
    for document in documents {
        collect(document, "", &mut owners);
    }
    let mut templates: Vec<String> = owners.keys().cloned().collect();
    templates.sort();
    let variants = [(300, "bb", "webp"), (64, "bb", "jpg"), (1200, "cc", "webp")];
    let (mut ok, mut missing) = (0, Vec::new());
    for template in &templates {
        for (i, (edge, crop, format)) in variants.iter().enumerate() {
            let concrete = cider_offline_core::artwork::sized_url(template, *edge)
                .replace("bb.jpg", &format!("{crop}.{format}"));
            let concrete = if template.contains("{f}") || template.contains("%7Bf%7D") {
                cider_offline_core::artwork::sized_url(
                    &template.replace("{f}", format).replace("{c}", crop),
                    *edge,
                )
            } else {
                concrete
            };
            let good = match client.get(&concrete).await {
                Ok(seen) => {
                    seen.status == 200
                        && seen.content_type.starts_with("image/")
                        && (i > 0 || image::load_from_memory(&seen.bytes).is_ok())
                }
                Err(_) => false,
            };
            let mut outcome = Outcome::default();
            if good {
                ok += 1;
            } else {
                outcome.fail(format!("{concrete} (referenced by {})", owners[template]));
                if missing.len() < 10 {
                    missing.push(concrete);
                }
            }
            report.record(
                "ARTWORK referenced by offline responses (3 sizes/formats each)",
                outcome,
            );
        }
    }
    report.content(
        "every artwork URL referenced by the offline responses resolves to image bytes",
        missing.is_empty(),
        format!(
            "{ok}/{} requests over {} distinct images {missing:?}",
            templates.len() * variants.len(),
            templates.len()
        ),
    );
}

// ----- Songs page and online interception --------------------------------------

/// Cider's Songs page query (from the captures), without `offset`.
fn songs_query(captures: &[Captured]) -> String {
    captured_query(
        captures,
        "GET amp-api.music.apple.com /v1/me/library/songs",
        "sort=",
    )
    .map_or_else(
        || {
            "limit=100&l=en-GB&include%5Blibrary-songs%5D=albums%2CpersonalRating&sort=dateAdded&extend=inFavorites"
                .to_owned()
        },
        |q| without_offset(&q),
    )
}

/// Real (non-custom) song ids of a listing.
fn real_ids(ids: Vec<String>) -> Vec<String> {
    ids.into_iter()
        .filter(|id| !id.starts_with("local-"))
        .collect()
}

/// The Songs page in both settings, offline.
async fn songs_page_checks(
    handle: &ProxyHandle,
    client: &Client,
    oracle: &Oracle,
    captures: &[Captured],
    report: &mut Report,
) -> anyhow::Result<()> {
    let query = songs_query(captures);
    let mut documents = Vec::new();
    let path = "/v1/me/library/songs";

    let (ids, total, problems) = page_all(client, path, &query, &mut documents).await?;
    let ids = real_ids(ids);
    report.content(
        "offline Songs page (apple) == Apple's synced Songs tab (ids + order)",
        !oracle.songs.is_empty() && ids == oracle.songs && problems.is_empty(),
        format!(
            "{} of {} songs, meta.total {total:?} {problems:?}",
            ids.len(),
            oracle.songs.len()
        ),
    );
    let (ids, _, problems) = page_all(client, path, "l=en-GB&limit=7", &mut documents).await?;
    let got: HashSet<String> = real_ids(ids).into_iter().collect();
    let expected: HashSet<String> = oracle.songs.iter().cloned().collect();
    report.content(
        "offline Songs page (apple) without sort, paged by 7: same songs",
        got == expected && problems.is_empty(),
        format!("{} of {} {problems:?}", got.len(), expected.len()),
    );

    handle.set_songs_page(SongsPage::All);
    let result = page_all(client, path, &query, &mut documents).await;
    handle.set_songs_page(SongsPage::Apple);
    let (ids, total, problems) = result?;
    let got: HashSet<String> = real_ids(ids).into_iter().collect();
    report.content(
        "offline Songs page (all) == every library song (setting applied live)",
        got == oracle.all_songs && got.len() > oracle.songs.len() && problems.is_empty(),
        format!(
            "{} of {} songs, meta.total {total:?}, {} missing, {} extra {problems:?}",
            got.len(),
            oracle.all_songs.len(),
            oracle.all_songs.difference(&got).count(),
            got.difference(&oracle.all_songs).count()
        ),
    );
    Ok(())
}

/// Cider's auth headers from the stored tokens (sensitive; never printed).
async fn auth_headers(storage: &Storage) -> anyhow::Result<reqwest::header::HeaderMap> {
    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, ORIGIN, REFERER};
    let developer = storage
        .get_auth_token(TokenKind::DeveloperToken)
        .await?
        .context("no developer token stored")?;
    let user = storage
        .get_auth_token(TokenKind::MediaUserToken)
        .await?
        .context("no media-user token stored")?;
    let developer = if developer
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
    {
        developer
    } else {
        format!("Bearer {developer}")
    };
    let mut headers = HeaderMap::new();
    let mut authorization = HeaderValue::from_str(&developer).context("invalid token")?;
    authorization.set_sensitive(true);
    let mut user = HeaderValue::from_str(&user).context("invalid token")?;
    user.set_sensitive(true);
    headers.insert(AUTHORIZATION, authorization);
    headers.insert("media-user-token", user);
    headers.insert(ORIGIN, HeaderValue::from_static("https://music.apple.com"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://music.apple.com/"),
    );
    Ok(headers)
}

/// Whether the proxy answered a response itself.
fn is_local(response: &reqwest::Response) -> bool {
    response.headers().contains_key("x-cider-offline")
}

const WEB_PLAYBACK: &str = "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/webPlayback";

/// The first asset URL of a `webPlayback` answer.
async fn asset_url(response: reqwest::Response) -> Option<String> {
    response.json::<Value>().await.ok().and_then(|v| {
        v.pointer("/songList/0/assets/0/URL")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

/// The Songs page while online, in both settings.
async fn online_songs_checks(
    handle: &ProxyHandle,
    client: &Client,
    oracle: &Oracle,
    captures: &[Captured],
    report: &mut Report,
) -> anyhow::Result<()> {
    let query = songs_query(captures);
    let path = "/v1/me/library/songs";
    let first = format!("{API}{path}?offset=0&{query}");
    let mut documents = Vec::new();

    // "apple": Apple's own, live answer, which must equal the synced list.
    let response = client.http.get(&first).send().await?;
    let forwarded = !is_local(&response) && response.status().is_success();
    let (ids, total, problems) = page_all(client, path, &query, &mut documents).await?;
    let ids = real_ids(ids);
    report.content(
        "online Songs page (apple): forwarded to Apple, Apple's live list == synced Songs tab",
        forwarded && ids == oracle.songs && problems.is_empty(),
        format!(
            "forwarded {forwarded}, Apple {} songs (meta.total {total:?}), synced {} {problems:?}",
            ids.len(),
            oracle.songs.len()
        ),
    );

    // "all": answered locally even though Apple is reachable.
    handle.set_songs_page(SongsPage::All);
    let response = client.http.get(&first).send().await;
    let paged = page_all(client, path, &query, &mut documents).await;
    // An id lookup is never intercepted, whatever the setting.
    let lookup_id = oracle.songs.first().cloned().unwrap_or_default();
    let lookup = client
        .http
        .get(format!("{API}{path}?ids={lookup_id}&l=en-GB"))
        .send()
        .await;
    handle.set_songs_page(SongsPage::Apple);
    let local = response.as_ref().is_ok_and(is_local);
    let (ids, _, problems) = paged?;
    let got: HashSet<String> = real_ids(ids).into_iter().collect();
    report.content(
        "online Songs page (all): answered locally with every library song",
        local && got == oracle.all_songs && problems.is_empty(),
        format!(
            "local {local}, {} of {} songs {problems:?}",
            got.len(),
            oracle.all_songs.len()
        ),
    );
    let lookup = lookup?;
    let lookup_forwarded = !is_local(&lookup) && lookup.status().is_success();
    let lookup_ids = lookup
        .json::<Value>()
        .await
        .map(|v| ids_of(&v))
        .unwrap_or_default();
    report.content(
        "online library song id lookup (all): forwarded to Apple",
        lookup_forwarded && lookup_ids == [lookup_id.clone()],
        format!("forwarded {lookup_forwarded}, {lookup_ids:?}"),
    );
    Ok(())
}

/// Playback while online: downloaded tracks stream from the local file,
/// others go to Apple untouched.
async fn online_playback_checks(
    client: &Client,
    oracle: &Oracle,
    report: &mut Report,
) -> anyhow::Result<()> {
    let synced_ids = || oracle.synced.iter().flat_map(|s| &s.track_ids);
    let play = |id: &str| {
        serde_json::json!({
            "universalLibraryId": id,
            "subscriptionAdamId": oracle.catalog_of.get(id).cloned().unwrap_or_default(),
        })
        .to_string()
    };
    let post = |id: &str| {
        client
            .http
            .post(WEB_PLAYBACK)
            .header("content-type", "application/json")
            .body(play(id))
            .send()
    };

    let downloaded =
        synced_ids().find(|id| oracle.is_downloaded(id) && oracle.catalog_of.contains_key(*id));
    if let Some(id) = downloaded {
        let response = post(id).await?;
        let local = is_local(&response);
        let url = asset_url(response).await;
        let media = match &url {
            Some(url) if url.contains("/__cider-offline/media/") => client
                .http
                .get(url)
                .header("range", "bytes=0-4095")
                .send()
                .await
                .ok(),
            _ => None,
        };
        let (status, content_type, bytes) = match media {
            Some(r) => {
                let status = r.status().as_u16();
                let ct = r
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let bytes = r.bytes().await.map_or(0, |b| b.len());
                (status, ct, bytes)
            }
            None => (0, String::new(), 0),
        };
        report.content(
            "online playback of a downloaded track: local media URL, served with Range",
            local && status == 206 && content_type.starts_with("audio/") && bytes == 4096,
            format!("{id}: local {local}, media {status} {content_type} {bytes} bytes"),
        );
    } else {
        report.content(
            "online playback of a downloaded track",
            false,
            "no downloaded synced track".to_owned(),
        );
    }

    let not_downloaded =
        synced_ids().find(|id| !oracle.is_downloaded(id) && oracle.catalog_of.contains_key(*id));
    if let Some(id) = not_downloaded {
        let response = post(id).await?;
        let local = is_local(&response);
        let status = response.status().as_u16();
        let url = asset_url(response).await;
        let to_apple = url
            .as_deref()
            .is_none_or(|u| !u.contains("/__cider-offline/"));
        report.content(
            "online playback of a not-downloaded track: forwarded to Apple untouched",
            !local && to_apple,
            format!("{id}: local {local}, Apple status {status}"),
        );
    } else {
        report.content(
            "online playback of a not-downloaded track",
            false,
            "no synced track without audio".to_owned(),
        );
    }
    Ok(())
}

/// The proxy switched online (connectivity monitor off, not forced offline).
async fn online_checks(
    handle: &ProxyHandle,
    oracle: &Oracle,
    captures: &[Captured],
    report: &mut Report,
) -> anyhow::Result<()> {
    let headers = match auth_headers(handle.storage()).await {
        Ok(headers) => headers,
        Err(error) => {
            report.content("online checks", false, format!("{error:#}"));
            return Ok(());
        }
    };
    let client = Client::with_headers(
        handle.local_addr(),
        handle.certificate_authority().cert_pem(),
        &storefront(captures),
        headers,
    )?;
    handle.set_force_offline(false);
    let songs = online_songs_checks(handle, &client, oracle, captures, report).await;
    let playback = online_playback_checks(&client, oracle, report).await;
    handle.set_force_offline(true);
    songs?;
    playback
}
