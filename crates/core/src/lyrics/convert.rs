//! From a provider's lyrics (LRC text or word-timed lines) to
//! [`FetchedLyrics`], dropping the credit lines the Chinese services put
//! in front of the sung text.

use super::matching::is_credit_line;
use super::ttml::{LrcLine, TimedLine, lines_to_ttml, parse_lrc, words_to_ttml};
use super::{FetchedLyrics, LyricsKind, LyricsQuality, LyricsQuery, plain_to_ttml};

/// Texts some services return instead of lyrics.
const NO_LYRICS_MARKERS: &[&str] = &[
    "纯音乐，请欣赏",
    "纯音乐, 请欣赏",
    "此歌曲为没有填词的纯音乐，请您欣赏",
];

fn is_placeholder(text: &str) -> bool {
    let text = text.trim();
    NO_LYRICS_MARKERS.iter().any(|m| text.contains(m))
}

/// Word-timed lines as word-synced TTML (stored as the line file: word-timed
/// `.syllable.ttml` files are Apple's alone). `None` when nothing sung is left.
pub(crate) fn from_words(
    lines: Vec<TimedLine>,
    query: &LyricsQuery,
    matched: String,
) -> Option<FetchedLyrics> {
    let lines: Vec<TimedLine> = lines
        .into_iter()
        .filter(|line| {
            let text = line.text();
            !text.is_empty() && !is_credit_line(&text, query) && !is_placeholder(&text)
        })
        .collect();
    let ttml = words_to_ttml(&lines, query.duration_ms.map(u64::from))?;
    Some(FetchedLyrics {
        ttml,
        kind: LyricsKind::Line,
        quality: LyricsQuality::Word,
        matched: Some(matched),
    })
}

/// LRC as line-synced TTML, or plain TTML when it carries no timestamps.
/// `None` when nothing sung is left.
pub(crate) fn from_lrc(lrc: &str, query: &LyricsQuery, matched: String) -> Option<FetchedLyrics> {
    if is_placeholder(lrc) && lrc.lines().filter(|l| !l.trim().is_empty()).count() <= 2 {
        return None;
    }
    let lines: Vec<LrcLine> = parse_lrc(lrc)
        .into_iter()
        .filter(|line| line.text.is_empty() || !is_credit_line(&line.text, query))
        .collect();
    if lines.iter().any(|line| !line.text.is_empty()) {
        let ttml = lines_to_ttml(&lines, query.duration_ms.map(u64::from))?;
        return Some(FetchedLyrics {
            ttml,
            kind: LyricsKind::Line,
            quality: LyricsQuality::Line,
            matched: Some(matched),
        });
    }
    // No timestamps at all: plain text (metadata tags dropped).
    let plain: Vec<&str> = lrc
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('[') && !l.starts_with('{'))
        .filter(|l| !is_credit_line(l, query) && !is_placeholder(l))
        .collect();
    let ttml = plain_to_ttml(&plain.join("\n"))?;
    Some(FetchedLyrics {
        ttml,
        kind: LyricsKind::Line,
        quality: LyricsQuality::Plain,
        matched: Some(matched),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::ttml::TimedWord;
    use crate::lyrics::ttml_quality;

    fn query() -> LyricsQuery {
        LyricsQuery {
            title: "Blinding Lights".to_owned(),
            artist: "The Weeknd".to_owned(),
            album: None,
            duration_ms: Some(200_000),
            isrc: None,
        }
    }

    #[test]
    fn lrc_should_drop_credits_and_be_line_quality() {
        let lrc = "[00:00.00]作词 : Max Martin\n[00:01.75]Lyrics by：Max\n[00:27.67]I've been tryna call\n[00:30.45]I've been on my own";
        let found = from_lrc(lrc, &query(), String::new()).unwrap();
        assert!(
            found.quality == LyricsQuality::Line
                && ttml_quality(&found.ttml) == LyricsQuality::Line
                && !found.ttml.contains("Max")
                && found.ttml.contains("<p begin=\"27.670\" end=\"30.450\"")
        );
    }

    #[test]
    fn untimed_lrc_should_be_plain() {
        let found = from_lrc("one\ntwo", &query(), String::new()).unwrap();
        assert_eq!(ttml_quality(&found.ttml), LyricsQuality::Plain);
    }

    #[test]
    fn instrumental_marker_should_be_none() {
        assert_eq!(
            from_lrc("[00:00.00]纯音乐，请欣赏", &query(), String::new()),
            None
        );
    }

    #[test]
    fn words_should_drop_credit_lines() {
        let line = |start, text: &str| TimedLine {
            start_ms: start,
            end_ms: start + 500,
            words: vec![TimedWord {
                start_ms: start,
                end_ms: start + 500,
                text: text.to_owned(),
            }],
        };
        let found = from_words(
            vec![
                line(0, "Blinding Lights - The Weeknd"),
                line(27_000, "call"),
            ],
            &query(),
            String::new(),
        )
        .unwrap();
        assert!(!found.ttml.contains("Weeknd") && found.ttml.contains(">call</span>"));
    }
}
