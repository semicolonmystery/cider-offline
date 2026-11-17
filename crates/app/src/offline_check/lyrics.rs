//! Lyrics checks of `--offline-check`: which lyrics Cider gets, offline and
//! online, and the `hasLyrics` / `hasTimeSyncedLyrics` flags of songs that
//! have local lyrics.
//!
//! Everything runs on the temporary copy: custom lyrics and seeded lyrics are
//! written only into the copy's lyrics directory. Custom lyrics are written
//! as files directly (their database label is covered by unit tests), so
//! the copied store's schema does not matter here.

use std::time::Duration;

use cider_offline_core::ProxyHandle;
use cider_offline_core::lyrics::{LyricsFlagsInterceptor, LyricsKind, LyricsQuality, LyricsStore};
use cider_offline_core::storage::Track;
use serde_json::{Value, json};

use super::{Client, Outcome, Report, auth_headers, is_local};

/// How many songs with local lyrics are fetched offline.
const SAMPLE: usize = 25;
/// Marker inside the custom lyrics written for the check.
const CUSTOM_MARKER: &str = "offline-check custom lyrics";
/// Line-synced TTML seeded for a song Apple says has no lyrics.
const SEEDED_TTML: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Line"><body><div><p begin="0.500" end="2.000">offline-check seeded lyrics</p></div></body></tt>"#;

fn custom_ttml() -> String {
    format!(
        r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Line"><body><div><p begin="0.500" end="2.000">{CUSTOM_MARKER}</p></div></body></tt>"#
    )
}

fn endpoint(kind: LyricsKind) -> &'static str {
    match kind {
        LyricsKind::Line => "lyrics",
        LyricsKind::Syllable => "syllable-lyrics",
    }
}

/// The lyrics URL exactly as Cider requests it (from captures): only
/// `syllable-lyrics` asks for `extend=ttmlLocalizations`.
fn lyrics_url(client: &Client, id: &str, kind: LyricsKind) -> String {
    let query = match kind {
        LyricsKind::Line => "platform=web&art%5Burl%5D=f",
        LyricsKind::Syllable => {
            "platform=web&art%5Burl%5D=f&l%5Blyrics%5D=en&l%5Bscript%5D=en-Latn&extend=ttmlLocalizations"
        }
    };
    format!("{}/songs/{id}/{}?{query}", client.catalog(), endpoint(kind))
}

/// Validates an Apple-shaped lyrics document for `path` (status already 2xx):
/// JSON, `data[0].type` of the requested kind, a TTML document under
/// `attributes.ttmlLocalizations` (or `attributes.ttml`, Apple's key when
/// not asked to extend). Returns the TTML.
pub(super) fn check_lyrics_body<'a>(
    content_type: &str,
    document: Option<&'a Value>,
    path: &str,
    outcome: &mut Outcome,
) -> Option<&'a str> {
    let path = path.split('?').next().unwrap_or(path);
    let kind = if path.ends_with("/syllable-lyrics") {
        "syllable-lyrics"
    } else {
        "lyrics"
    };
    if !content_type.starts_with("application/json") {
        outcome.fail(format!("lyrics content-type {content_type:?}"));
        return None;
    }
    let Some(document) = document else {
        outcome.fail("lyrics: invalid JSON");
        return None;
    };
    let resource_type = document.pointer("/data/0/type").and_then(Value::as_str);
    if resource_type != Some(kind) {
        outcome.fail(format!("lyrics type {resource_type:?}, expected {kind:?}"));
    }
    let ttml = document
        .pointer("/data/0/attributes/ttmlLocalizations")
        .and_then(|value| match value {
            Value::String(ttml) => Some(ttml.as_str()),
            Value::Object(map) => map.values().find_map(Value::as_str),
            _ => None,
        })
        .or_else(|| {
            document
                .pointer("/data/0/attributes/ttml")
                .and_then(Value::as_str)
        })
        .filter(|ttml| ttml.trim_start().starts_with('<') && ttml.contains("<tt"));
    if ttml.is_none() {
        outcome.fail("lyrics: no TTML document under attributes.ttmlLocalizations / ttml");
    }
    ttml
}

/// One lyrics-relevant response as Cider would see it.
struct Answer {
    status: u16,
    local: bool,
    content_type: String,
    document: Option<Value>,
}

impl Answer {
    /// The TTML of a valid lyrics answer, `None` (with the reason) otherwise.
    fn ttml(&self, path: &str) -> Result<String, String> {
        if !(200..300).contains(&self.status) {
            return Err(format!("status {}", self.status));
        }
        let mut outcome = Outcome::default();
        check_lyrics_body(
            &self.content_type,
            self.document.as_ref(),
            path,
            &mut outcome,
        )
        .map(str::to_owned)
        .ok_or_else(|| outcome.failures.join("; "))
    }

    fn attribute(&self, pointer: &str) -> Option<&Value> {
        self.document.as_ref()?.pointer(pointer)
    }
}

async fn fetch(client: &Client, url: &str) -> anyhow::Result<Answer> {
    let response = client.http.get(url).send().await?;
    let status = response.status().as_u16();
    let local = is_local(&response);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let document = response.json::<Value>().await.ok();
    Ok(Answer {
        status,
        local,
        content_type,
        document,
    })
}

/// A stored song whose Apple metadata says it has no (synced) lyrics.
struct Candidate {
    /// Id the lyrics files and catalog requests use.
    lyrics_id: String,
    /// The Apple resource as stored.
    resource: Value,
}

fn apple_flag(resource: &Value, key: &str) -> bool {
    resource
        .pointer(&format!("/attributes/{key}"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Catalog songs whose stored Apple resource (catalog or library) says
/// `hasLyrics: false`, catalog-native rows first.
fn without_apple_lyrics(tracks: &[Track]) -> Vec<Candidate> {
    let mut candidates: Vec<(bool, Candidate)> = tracks
        .iter()
        .filter(|t| !t.is_custom)
        .filter_map(|track| {
            let lyrics_id = track.catalog_id.clone().unwrap_or_else(|| track.id.clone());
            if lyrics_id.is_empty() || !lyrics_id.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let resource: Value = serde_json::from_str(track.metadata.as_deref()?).ok()?;
            let kind = resource.get("type").and_then(Value::as_str);
            let says_none = resource.pointer("/attributes/hasLyrics") == Some(&json!(false));
            (matches!(kind, Some("songs" | "library-songs")) && says_none).then(|| {
                (
                    track.id != lyrics_id,
                    Candidate {
                        lyrics_id,
                        resource,
                    },
                )
            })
        })
        .collect();
    candidates.sort_by_key(|(library, _)| *library);
    candidates.into_iter().map(|(_, c)| c).collect()
}

/// Why no candidate was found (counts over the stored tracks).
fn candidate_diagnostics(tracks: &[Track]) -> String {
    let mut with_metadata = 0;
    let mut flag = [0usize; 3];
    for track in tracks {
        let Some(resource) = track
            .metadata
            .as_deref()
            .and_then(|m| serde_json::from_str::<Value>(m).ok())
        else {
            continue;
        };
        with_metadata += 1;
        let slot = match resource.pointer("/attributes/hasLyrics") {
            Some(Value::Bool(true)) => 0,
            Some(Value::Bool(false)) => 1,
            _ => 2,
        };
        flag[slot] += 1;
    }
    format!(
        "{} tracks, {with_metadata} with Apple metadata: hasLyrics true {}, false {}, absent {}",
        tracks.len(),
        flag[0],
        flag[1],
        flag[2]
    )
}

/// The checks; see the [module docs](self).
pub(super) async fn lyrics_checks(
    handle: &ProxyHandle,
    client: &Client,
    report: &mut Report,
) -> anyhow::Result<()> {
    let store = handle.library().lyrics_store();
    store.load_index().await;
    let indexed: Vec<(String, LyricsQuality)> = store
        .indexed()
        .into_iter()
        .filter(|(id, _)| id.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    report.content(
        "lyrics index: songs with local lyrics",
        !indexed.is_empty(),
        format!("{} catalog songs with local lyrics", indexed.len()),
    );

    offline_served_checks(client, &store, &indexed, report).await?;
    let tracks = handle.storage().list_tracks().await?;
    let flagged = flags_candidate(&store, &tracks).await?;
    if let Some(candidate) = &flagged {
        offline_flags_checks(client, &store, candidate, report).await?;
    } else {
        report.content(
            "lyrics flags: a stored song Apple says has no lyrics",
            false,
            candidate_diagnostics(&tracks),
        );
    }

    // Custom lyrics: on a song that also has Apple / provider lyrics.
    let custom_id = indexed
        .iter()
        .find(|(_, quality)| *quality == LyricsQuality::Word)
        .or_else(|| indexed.first())
        .map(|(id, _)| id.clone());
    let Some(custom_id) = custom_id else {
        report.content(
            "custom lyrics",
            false,
            "no song with local lyrics".to_owned(),
        );
        return Ok(());
    };
    let custom = custom_ttml();
    store.write_custom(&custom_id, &custom).await?;
    custom_checks(client, &custom_id, "offline", report).await?;

    online_checks(
        handle,
        client,
        &store,
        &indexed,
        &custom_id,
        flagged.as_ref(),
        report,
    )
    .await
}

/// Every sampled song with local lyrics is served offline on both endpoints,
/// in Apple's shape, with exactly the stored TTML.
async fn offline_served_checks(
    client: &Client,
    store: &LyricsStore,
    indexed: &[(String, LyricsQuality)],
    report: &mut Report,
) -> anyhow::Result<()> {
    let mut served = 0;
    let mut failures = Vec::new();
    for (id, _) in indexed.iter().take(SAMPLE) {
        for kind in [LyricsKind::Line, LyricsKind::Syllable] {
            let url = lyrics_url(client, id, kind);
            let answer = fetch(client, &url).await?;
            let expected = store.read(id, kind).await.map(|(ttml, _)| ttml);
            match answer.ttml(&url) {
                Ok(ttml) if Some(&ttml) == expected.as_ref() && answer.local => served += 1,
                Ok(_) => failures.push(format!("{id}/{}: not the stored TTML", endpoint(kind))),
                Err(reason) => failures.push(format!("{id}/{}: {reason}", endpoint(kind))),
            }
        }
    }
    report.content(
        "offline lyrics: local TTML in Apple's shape on both endpoints",
        failures.is_empty() && served > 0,
        if failures.is_empty() {
            format!("{served} answers checked")
        } else {
            failures.join(", ")
        },
    );
    Ok(())
}

/// A song Apple says has no lyrics but that has local synced lyrics; seeds
/// lyrics into the copy for one when the store has none such.
async fn flags_candidate(
    store: &LyricsStore,
    tracks: &[Track],
) -> anyhow::Result<Option<Candidate>> {
    let mut candidates = without_apple_lyrics(tracks);
    if let Some(index) = candidates
        .iter()
        .position(|c| store.quality(&c.lyrics_id) >= Some(LyricsQuality::Line))
    {
        return Ok(Some(candidates.swap_remove(index)));
    }
    let Some(candidate) = candidates.into_iter().next() else {
        return Ok(None);
    };
    store
        .write(&candidate.lyrics_id, LyricsKind::Line, SEEDED_TTML)
        .await?;
    Report::line(&format!(
        "lyrics flags: seeded line-synced lyrics for {} in the copy (no song had local lyrics but Apple's hasLyrics false)",
        candidate.lyrics_id
    ));
    Ok(Some(candidate))
}

fn flags_ok(answer: &Answer, pointer: &str, apple: &Value) -> Result<(), String> {
    let get = |key: &str| answer.attribute(&format!("{pointer}/attributes/{key}"));
    if !(200..300).contains(&answer.status) {
        return Err(format!("status {}", answer.status));
    }
    if get("hasLyrics") != Some(&json!(true)) {
        return Err(format!("hasLyrics {:?}", get("hasLyrics")));
    }
    if get("hasTimeSyncedLyrics") != Some(&json!(true)) {
        return Err(format!(
            "hasTimeSyncedLyrics {:?}",
            get("hasTimeSyncedLyrics")
        ));
    }
    let rating = apple.pointer("/attributes/contentRating");
    if rating.is_some() && get("contentRating") != rating {
        return Err(format!(
            "contentRating changed to {:?}",
            get("contentRating")
        ));
    }
    Ok(())
}

/// The catalog requests Cider makes for one song, with where the song is.
fn song_requests(client: &Client, id: &str) -> [(String, String); 3] {
    let catalog = client.catalog();
    [
        (
            format!("{catalog}/songs/{id}?l=en-GB"),
            "/data/0".to_owned(),
        ),
        (
            format!("{catalog}/songs?ids={id}&l=en-GB"),
            "/data/0".to_owned(),
        ),
        (
            format!("{catalog}?l=en-GB&ids%5Bsongs%5D={id}&format%5Bresources%5D=map"),
            format!("/resources/songs/{id}"),
        ),
    ]
}

/// Offline responses and a simulated online rewrite set both flags.
async fn offline_flags_checks(
    client: &Client,
    store: &LyricsStore,
    candidate: &Candidate,
    report: &mut Report,
) -> anyhow::Result<()> {
    let id = &candidate.lyrics_id;
    let mut failures = Vec::new();
    for (url, pointer) in song_requests(client, id) {
        let answer = fetch(client, &url).await?;
        if let Err(reason) = flags_ok(&answer, &pointer, &candidate.resource) {
            failures.push(format!("{url}: {reason}"));
        }
    }
    report.content(
        "offline lyrics flags: Apple says hasLyrics false, local synced lyrics -> true",
        failures.is_empty(),
        if failures.is_empty() {
            format!("{id}: songs/{{id}}, songs?ids=, map lookup")
        } else {
            failures.join(", ")
        },
    );

    // The exact rewrite the proxy applies online, on Apple's stored resource.
    let mut document = json!({"data": [candidate.resource.clone()]});
    let changed = LyricsFlagsInterceptor::new(store.clone()).annotate(&mut document);
    let mut rewritten = document.pointer("/data/0").cloned().unwrap_or_default();
    let flags = (
        apple_flag(&rewritten, "hasLyrics"),
        apple_flag(&rewritten, "hasTimeSyncedLyrics"),
    );
    // Nothing but the two flags may differ (here or in nested song
    // resources, e.g. a library song's catalog twin).
    let mut original = candidate.resource.clone();
    strip_flags(&mut rewritten);
    strip_flags(&mut original);
    report.content(
        "simulated online rewrite of Apple's resource sets only the lyrics flags",
        changed && flags == (true, true) && rewritten == original,
        format!("{id}: changed {changed}, flags {flags:?}"),
    );
    Ok(())
}

/// Removes both lyrics flags from every object in `value`.
fn strip_flags(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("hasLyrics");
            map.remove("hasTimeSyncedLyrics");
            map.values_mut().for_each(strip_flags);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_flags),
        _ => {}
    }
}

/// The custom lyrics of `id` reach Cider on both endpoints, locally.
async fn custom_checks(
    client: &Client,
    id: &str,
    when: &str,
    report: &mut Report,
) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for kind in [LyricsKind::Line, LyricsKind::Syllable] {
        let url = lyrics_url(client, id, kind);
        let answer = fetch(client, &url).await?;
        match answer.ttml(&url) {
            Ok(ttml) if ttml.contains(CUSTOM_MARKER) && answer.local => {}
            Ok(_) => failures.push(format!(
                "{}: not the custom lyrics (local {})",
                endpoint(kind),
                answer.local
            )),
            Err(reason) => failures.push(format!("{}: {reason}", endpoint(kind))),
        }
    }
    report.content(
        &format!("{when} custom lyrics win on both endpoints"),
        failures.is_empty(),
        if failures.is_empty() {
            format!("{id}: served locally on lyrics and syllable-lyrics")
        } else {
            format!("{id}: {}", failures.join(", "))
        },
    );
    Ok(())
}

/// Online: custom lyrics still win and stay on disk, "Always use downloaded
/// lyrics" serves local lyrics live, otherwise Apple answers and its lyrics
/// are saved, and Apple's own `hasLyrics: false` is rewritten.
async fn online_checks(
    handle: &ProxyHandle,
    offline_client: &Client,
    store: &LyricsStore,
    indexed: &[(String, LyricsQuality)],
    custom_id: &str,
    flagged: Option<&Candidate>,
    report: &mut Report,
) -> anyhow::Result<()> {
    let headers = match auth_headers(handle.storage()).await {
        Ok(headers) => headers,
        Err(error) => {
            report.content("online lyrics checks", false, format!("{error:#}"));
            return Ok(());
        }
    };
    let client = Client::with_headers(
        handle.local_addr(),
        handle.certificate_authority().cert_pem(),
        &offline_client.storefront,
        headers,
    )?;
    handle.set_force_offline(false);
    let result = online_steps(handle, &client, store, indexed, custom_id, flagged, report).await;
    handle.set_prefer_local_lyrics(false);
    handle.set_force_offline(true);
    result
}

async fn online_steps(
    handle: &ProxyHandle,
    client: &Client,
    store: &LyricsStore,
    indexed: &[(String, LyricsQuality)],
    custom_id: &str,
    flagged: Option<&Candidate>,
    report: &mut Report,
) -> anyhow::Result<()> {
    // 1. Custom lyrics win online with the setting off; the file survives.
    handle.set_prefer_local_lyrics(false);
    custom_checks(client, custom_id, "online (setting off)", report).await?;
    let kept = store
        .read_custom(custom_id)
        .await
        .is_some_and(|ttml| ttml.contains(CUSTOM_MARKER));
    report.content(
        "online: custom lyrics on disk untouched",
        kept,
        custom_id.to_owned(),
    );

    // 2. Setting off: Apple answers (word-timed song), and its lyrics are saved.
    let apple_id = indexed
        .iter()
        .find(|(id, quality)| *quality == LyricsQuality::Word && id != custom_id)
        .map(|(id, _)| id.clone());
    if let Some(id) = &apple_id {
        let url = lyrics_url(client, id, LyricsKind::Syllable);
        let answer = fetch(client, &url).await?;
        let (ok, detail) = match answer.ttml(&url) {
            Ok(ttml) if !answer.local => {
                let saved = wait_saved(store, id, &ttml).await;
                (
                    saved,
                    format!("{id}: Apple answered, saved locally {saved}"),
                )
            }
            Ok(_) => (
                true,
                format!("{id}: Apple had none for this storefront; local fallback served"),
            ),
            Err(reason) => (false, format!("{id}: {reason}")),
        };
        report.content(
            "online (setting off): lyrics from Apple, saved locally",
            ok,
            detail,
        );
    }

    // 3. Setting on: local lyrics served without asking Apple, applied live.
    handle.set_prefer_local_lyrics(true);
    let local_id = apple_id.as_deref().or_else(|| {
        indexed
            .iter()
            .map(|(id, _)| id.as_str())
            .find(|id| *id != custom_id)
    });
    if let Some(id) = local_id {
        let url = lyrics_url(client, id, LyricsKind::Line);
        let answer = fetch(client, &url).await?;
        let expected = store.read(id, LyricsKind::Line).await.map(|(t, _)| t);
        let ok = answer.local && answer.ttml(&url).ok() == expected;
        report.content(
            "online (always use downloaded lyrics): local lyrics without asking Apple",
            ok,
            format!("{id}: local {}", answer.local),
        );
    }
    custom_checks(client, custom_id, "online (setting on)", report).await?;
    handle.set_prefer_local_lyrics(false);

    // 4. Apple's hasLyrics false is rewritten in the live answer.
    if let Some(candidate) = flagged {
        let mut failures = Vec::new();
        let mut from_apple = 0;
        for (url, pointer) in song_requests(client, &candidate.lyrics_id) {
            let answer = fetch(client, &url).await?;
            from_apple += usize::from(!answer.local);
            if let Err(reason) = flags_ok(&answer, &pointer, &candidate.resource) {
                failures.push(format!("{url}: {reason}"));
            }
        }
        report.content(
            "online lyrics flags: Apple's live answer rewritten to hasLyrics true",
            failures.is_empty() && from_apple > 0,
            if failures.is_empty() {
                format!("{}: {from_apple}/3 answers from Apple", candidate.lyrics_id)
            } else {
                failures.join(", ")
            },
        );
    }
    Ok(())
}

/// Waits (briefly) until the background save of Apple's lyrics landed.
async fn wait_saved(store: &LyricsStore, id: &str, ttml: &str) -> bool {
    for _ in 0..40 {
        if store
            .read_provider(id, LyricsKind::Syllable)
            .await
            .is_some_and(|(saved, _)| saved == ttml)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome_of(content_type: &str, document: &Value, path: &str) -> (bool, Vec<String>) {
        let mut outcome = Outcome::default();
        let ttml = check_lyrics_body(content_type, Some(document), path, &mut outcome);
        (ttml.is_some(), outcome.failures)
    }

    #[test]
    fn lyrics_body_check_should_accept_apple_shape() {
        let document = json!({"data": [{"id": "1", "type": "syllable-lyrics",
            "attributes": {"ttmlLocalizations": "<tt></tt>"}}]});
        assert_eq!(
            outcome_of(
                "application/json",
                &document,
                "/v1/catalog/us/songs/1/syllable-lyrics"
            ),
            (true, Vec::new())
        );
    }

    #[test]
    fn lyrics_body_check_should_reject_wrong_kind() {
        let document = json!({"data": [{"id": "1", "type": "lyrics",
            "attributes": {"ttmlLocalizations": "<tt></tt>"}}]});
        let (_, failures) = outcome_of(
            "application/json",
            &document,
            "/v1/catalog/us/songs/1/syllable-lyrics",
        );
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn lyrics_body_check_should_reject_missing_ttml() {
        let document = json!({"data": [{"id": "1", "type": "lyrics", "attributes": {}}]});
        assert!(!outcome_of("application/json", &document, "/songs/1/lyrics").0);
    }

    #[test]
    fn candidates_should_need_apple_saying_no_lyrics() {
        let mut with = Track::minimal("1", "A", "B");
        with.metadata = Some(
            json!({"id": "1", "type": "songs", "attributes": {"hasLyrics": true}}).to_string(),
        );
        let mut without = Track::minimal("2", "A", "B");
        without.metadata = Some(
            json!({"id": "2", "type": "songs", "attributes": {"hasLyrics": false}}).to_string(),
        );
        let ids: Vec<String> = without_apple_lyrics(&[with, without])
            .into_iter()
            .map(|c| c.lyrics_id)
            .collect();
        assert_eq!(ids, ["2"]);
    }
}
