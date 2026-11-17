//! Matching search results of the unofficial lyrics APIs to a track.
//!
//! A wrong song's lyrics are worse than none, so a candidate is accepted
//! only when its title and artist match after normalisation (case, accents,
//! punctuation, "feat." credits and version suffixes dropped) and its
//! duration is within [`DURATION_TOLERANCE_MS`] when both sides know it.
//! Versions the track is not (live, remix, instrumental, karaoke…) are
//! rejected.

use unicode_normalization::UnicodeNormalization as _;
use unicode_normalization::char::is_combining_mark;

use super::LyricsQuery;

/// Largest accepted duration difference.
pub(crate) const DURATION_TOLERANCE_MS: u64 = 3_000;
/// Smallest title / artist similarity (0–1) accepted for a non-exact match.
const MIN_SIMILARITY: f64 = 0.85;

/// Words marking a different recording or arrangement than the original.
const VERSION_MARKERS: &[&str] = &[
    "live",
    "remix",
    "instrumental",
    "karaoke",
    "acoustic",
    "cover",
    "sped up",
    "slowed",
    "nightcore",
    "8d",
    "demo",
    "伴奏",
    "纯音乐",
    "翻唱",
    "现场",
];

/// One search result, as the provider describes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    /// Song title.
    pub(crate) title: &'a str,
    /// Artist names (one string may hold several, e.g. `"A / B"`).
    pub(crate) artists: &'a [&'a str],
    /// Duration in milliseconds, when known.
    pub(crate) duration_ms: Option<u64>,
}

/// Lower-case, accent-free text with punctuation turned into single spaces.
pub(crate) fn normalize(text: &str) -> String {
    let folded: String = text
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '&' | '+' => ' ',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The title without bracketed parts, a `" - …"` suffix or a `feat.` credit:
/// `"Song (feat. X) - Remastered 2011"` → `"song"`.
pub(crate) fn core_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' | '[' | '（' | '【' => depth += 1,
            ')' | ']' | '）' | '】' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let base = out.split(" - ").next().unwrap_or(&out);
    let base = cut_featuring(base);
    let core = normalize(base);
    if core.is_empty() {
        normalize(title)
    } else {
        core
    }
}

/// Text before a `feat.` / `ft.` / `featuring` credit.
fn cut_featuring(text: &str) -> &str {
    let lower = text.to_lowercase();
    [" feat. ", " feat ", " ft. ", " ft ", " featuring "]
        .iter()
        .filter_map(|marker| lower.find(marker))
        .min()
        .and_then(|at| text.get(..at))
        .unwrap_or(text)
}

/// Single artist names of an artist credit (`"A, B & C feat. D"`).
fn artist_names(credit: &str) -> Vec<String> {
    let mut names = Vec::new();
    let lower = credit.to_lowercase();
    for part in lower
        .replace(" feat. ", ",")
        .replace(" feat ", ",")
        .replace(" ft. ", ",")
        .replace(" featuring ", ",")
        .replace(" with ", ",")
        .replace(" x ", ",")
        .replace(" and ", ",")
        .split([',', '&', '/', ';', '、', '+'])
    {
        let name = normalize(part);
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    let whole = normalize(credit);
    if !whole.is_empty() && !names.contains(&whole) {
        names.push(whole);
    }
    names
}

/// Similarity of two strings: 1 − Levenshtein distance / longer length.
pub(crate) fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if ca == cb {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[j])
            };
            diagonal = above;
        }
    }
    let distance = row[b.len()];
    // Lengths of song titles fit f64 exactly.
    #[expect(clippy::cast_precision_loss, reason = "short strings")]
    let ratio = distance as f64 / longest as f64;
    1.0 - ratio
}

/// Whether `text` (normalised) names a version marker as a whole word.
fn has_marker(text: &str, marker: &str) -> bool {
    if !marker.is_ascii() {
        return text.contains(marker);
    }
    let padded = format!(" {text} ");
    padded.contains(&format!(" {marker} "))
}

/// Score of the title match (`None`: no match).
fn title_score(wanted: &str, found: &str) -> Option<u32> {
    let (want_full, found_full) = (normalize(wanted), normalize(found));
    for marker in VERSION_MARKERS {
        if has_marker(&found_full, marker) && !has_marker(&want_full, marker) {
            return None;
        }
    }
    if want_full == found_full {
        return Some(100);
    }
    let (want_core, found_core) = (core_title(wanted), core_title(found));
    if want_core == found_core {
        return Some(90);
    }
    let sim = similarity(&want_core, &found_core);
    // Truncation to a whole score is intended.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..=80"
    )]
    let score = (sim * 80.0) as u32;
    (sim >= MIN_SIMILARITY).then_some(score)
}

/// Score of the artist match (`None`: no match).
fn artist_score(wanted: &str, found: &[&str]) -> Option<u32> {
    let want = artist_names(wanted);
    let found: Vec<String> = found.iter().flat_map(|a| artist_names(a)).collect();
    let mut best = None;
    for w in &want {
        for f in &found {
            let score = if w == f {
                100
            } else if (w.len() >= 3 && f.contains(w.as_str()))
                || (f.len() >= 3 && w.contains(f.as_str()))
            {
                80
            } else if similarity(w, f) >= MIN_SIMILARITY {
                70
            } else {
                continue;
            };
            best = best.max(Some(score));
        }
    }
    best
}

/// Score of `candidate` for `query`, higher is better; `None` rejects it.
pub(crate) fn score(query: &LyricsQuery, candidate: &Candidate<'_>) -> Option<u32> {
    let title = title_score(&query.title, candidate.title)?;
    let artist = artist_score(&query.artist, candidate.artists)?;
    let wanted = query.duration_ms.map(u64::from).filter(|d| *d > 0);
    let duration = match (wanted, candidate.duration_ms.filter(|d| *d > 0)) {
        (Some(wanted), Some(found)) => {
            let diff = wanted.abs_diff(found);
            if diff > DURATION_TOLERANCE_MS {
                return None;
            }
            // 0..=30 points, more for a closer duration.
            u32::try_from(30 - diff * 30 / (DURATION_TOLERANCE_MS + 1)).unwrap_or(0)
        }
        // Unknown duration: only near-exact names are trusted.
        _ if title < 90 || artist < 80 => return None,
        _ => 0,
    };
    Some(title + artist + duration)
}

/// The best-scoring accepted candidate.
pub(crate) fn best<'c, T>(
    query: &LyricsQuery,
    candidates: impl IntoIterator<Item = (T, Candidate<'c>)>,
) -> Option<T> {
    candidates
        .into_iter()
        .filter_map(|(item, candidate)| score(query, &candidate).map(|s| (s, item)))
        // `max_by_key` keeps the last maximum; reverse order keeps the first.
        .fold(None::<(u32, T)>, |best, (s, item)| match best {
            Some((top, _)) if top >= s => best,
            _ => Some((s, item)),
        })
        .map(|(_, item)| item)
}

/// Whether a lyric line is a credit ("作词 : X", "Lyrics by：X", "Title - Artist")
/// rather than sung text. Chinese services put these at the top.
pub(crate) fn is_credit_line(text: &str, query: &LyricsQuery) -> bool {
    const KEYS: &[&str] = &[
        "作词",
        "作曲",
        "编曲",
        "制作人",
        "制作",
        "监制",
        "词",
        "曲",
        "混音",
        "母带",
        "录音",
        "和声",
        "吉他",
        "贝斯",
        "鼓",
        "出品",
        "发行",
        "企划",
        "op",
        "sp",
        "lyrics by",
        "lyricist",
        "lyrics",
        "composed by",
        "composer",
        "music by",
        "written by",
        "writer",
        "writers",
        "produced by",
        "producer",
        "producers",
        "arranged by",
        "arranger",
        "mixed by",
        "mixing",
        "mastered by",
        "mastering",
        "recorded by",
        "recording",
        "vocals",
        "vocal",
    ];
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return true;
    }
    if let Some((key, _)) = trimmed.split_once([':', '：'])
        && key.chars().count() <= 20
        && KEYS.contains(&key.trim().to_lowercase().as_str())
    {
        return true;
    }
    // "Title - Artist" header line.
    if let Some((left, right)) = trimmed.split_once(" - ") {
        let (left, right) = (normalize(left), normalize(right));
        let (title, artist) = (normalize(&query.title), normalize(&query.artist));
        return (left == title && right == artist) || (left == artist && right == title);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(title: &str, artist: &str, secs: u32) -> LyricsQuery {
        LyricsQuery {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration_ms: Some(secs * 1_000),
            isrc: None,
        }
    }

    fn candidate<'a>(title: &'a str, artists: &'a [&'a str], secs: u64) -> Candidate<'a> {
        Candidate {
            title,
            artists,
            duration_ms: Some(secs * 1_000),
        }
    }

    #[test]
    fn normalize_should_drop_accents_case_and_punctuation() {
        assert_eq!(normalize("Crème Brûlée – Café!"), "creme brulee cafe");
    }

    #[test]
    fn core_title_should_drop_versions_and_features() {
        assert_eq!(core_title("Song (feat. X) - Remastered 2011"), "song");
    }

    #[test]
    fn exact_match_should_be_accepted() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        assert!(score(&q, &candidate("Blinding Lights", &["The Weeknd"], 200)).is_some());
    }

    #[test]
    fn duration_outside_tolerance_should_be_rejected() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        assert!(score(&q, &candidate("Blinding Lights", &["The Weeknd"], 253)).is_none());
    }

    #[test]
    fn other_artist_should_be_rejected() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        assert!(score(&q, &candidate("Blinding Lights", &["Efb Deejays"], 200)).is_none());
    }

    #[test]
    fn live_version_should_be_rejected() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        assert!(
            score(
                &q,
                &candidate("Blinding Lights (Live)", &["The Weeknd"], 201)
            )
            .is_none()
        );
    }

    #[test]
    fn featured_artist_in_title_should_still_match() {
        let q = query("Carnival", "Kanye West, Ty Dolla $ign", 264);
        let found = candidate("Carnival (feat. Rich The Kid)", &["¥$", "Kanye West"], 264);
        assert!(score(&q, &found).is_some());
    }

    #[test]
    fn accents_should_not_prevent_a_match() {
        let q = query("Šampaňský", "Řezník", 180);
        assert!(score(&q, &candidate("Sampansky", &["Reznik"], 181)).is_some());
    }

    #[test]
    fn unknown_duration_should_need_exact_names() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        let found = Candidate {
            title: "Blinding Light",
            artists: &["The Weeknd"],
            duration_ms: None,
        };
        assert!(score(&q, &found).is_none());
    }

    #[test]
    fn best_should_prefer_closer_duration() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        let picked = best(
            &q,
            [
                (1, candidate("Blinding Lights", &["The Weeknd"], 203)),
                (2, candidate("Blinding Lights", &["The Weeknd"], 200)),
            ],
        );
        assert_eq!(picked, Some(2));
    }

    #[test]
    fn credit_lines_should_be_detected() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        assert!(
            [
                "作词 : Max Martin",
                "Lyrics by：Max",
                "Blinding Lights - The Weeknd"
            ]
            .iter()
            .all(|line| is_credit_line(line, &q))
        );
    }

    #[test]
    fn sung_line_should_not_be_a_credit() {
        let q = query("Blinding Lights", "The Weeknd", 200);
        assert!(!is_credit_line(
            "I said, ooh, I'm blinded by the lights",
            &q
        ));
    }
}
