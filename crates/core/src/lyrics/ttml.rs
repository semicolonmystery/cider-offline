//! LRC parsing, word-timed lines, and conversion to Apple-style TTML.

use std::fmt::Write as _;

use crate::storage::LyricsQuality;

/// Namespaces / root attributes Apple uses for lyric TTML documents.
const TT_OPEN: &str = "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
    xmlns:itunes=\"http://music.apple.com/lyric-ttml-internal\" \
    xmlns:ttm=\"http://www.w3.org/ns/ttml#metadata\"";

/// How long the last synced line stays visible when nothing follows it.
const LAST_LINE_MS: u64 = 5_000;

/// One timed LRC line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LrcLine {
    /// Start time in milliseconds.
    pub(crate) start_ms: u64,
    /// Text with word-timing tags removed; may be empty (instrumental gap).
    pub(crate) text: String,
}

/// Parses `mm:ss`, `mm:ss.xx`, `mm:ss.xxx` or `mm:ss:xx` into milliseconds.
fn parse_timestamp(tag: &str) -> Option<u64> {
    let (minutes, rest) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let (seconds, fraction) = match rest.split_once(['.', ':']) {
        Some((s, f)) => (s, Some(f)),
        None => (rest, None),
    };
    let seconds: u64 = seconds.trim().parse().ok()?;
    if seconds >= 60 {
        return None;
    }
    let fraction_ms = match fraction {
        None => 0,
        Some(f) if !f.is_empty() && f.len() <= 3 && f.bytes().all(|b| b.is_ascii_digit()) => {
            let value: u64 = f.parse().ok()?;
            match f.len() {
                1 => value * 100,
                2 => value * 10,
                _ => value,
            }
        }
        Some(_) => return None,
    };
    Some(minutes * 60_000 + seconds * 1_000 + fraction_ms)
}

/// Removes enhanced-LRC word timings (`<mm:ss.xx>`).
fn strip_word_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        let inner = &rest[open + 1..open + close];
        out.push_str(&rest[..open]);
        if parse_timestamp(inner).is_none() {
            out.push_str(&rest[open..=open + close]);
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses synced LRC text. Lines may carry several timestamps; `[offset:±ms]`
/// is applied (positive shows lyrics earlier); other metadata tags are
/// ignored. The result is sorted by start time.
pub(crate) fn parse_lrc(lrc: &str) -> Vec<LrcLine> {
    let mut offset_ms: i64 = 0;
    let mut lines = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(tag_end) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
            let tag = &rest[1..=tag_end];
            if let Some(ms) = parse_timestamp(tag) {
                stamps.push(ms);
            } else if let Some(value) = tag.strip_prefix("offset:") {
                offset_ms = value.trim().parse().unwrap_or(0);
            }
            rest = rest[tag_end + 2..].trim_start();
        }
        let text = strip_word_tags(rest);
        for start in stamps {
            let shifted = i64::try_from(start).unwrap_or(i64::MAX) - offset_ms;
            lines.push(LrcLine {
                start_ms: u64::try_from(shifted.max(0)).unwrap_or(0),
                text: text.clone(),
            });
        }
    }
    lines.sort_by_key(|line| line.start_ms);
    lines
}

/// Formats milliseconds the way Apple's TTML does (`17.800`, `4:11.250`, `1:02:03.000`).
pub(crate) fn format_time(ms: u64) -> String {
    let (hours, rem) = (ms / 3_600_000, ms % 3_600_000);
    let (minutes, rem) = (rem / 60_000, rem % 60_000);
    let (seconds, millis) = (rem / 1_000, rem % 1_000);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}.{millis:03}")
    } else if minutes > 0 {
        format!("{minutes}:{seconds:02}.{millis:03}")
    } else {
        format!("{seconds}.{millis:03}")
    }
}

/// Escapes text for XML element content (quotes stay as they are, like in
/// Apple's own files; not for attribute values).
pub(crate) fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c if c.is_control() && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// Converts synced LRC into line-timed TTML. Each line ends where the next
/// timestamp (including empty gap lines) begins; the last line ends at the
/// track end (if known and later) or [`LAST_LINE_MS`] after it starts.
///
/// Returns `None` when the LRC has no timed, non-empty line.
pub(crate) fn lrc_to_ttml(lrc: &str, duration_ms: Option<u64>) -> Option<String> {
    lines_to_ttml(&parse_lrc(lrc), duration_ms)
}

/// [`lrc_to_ttml`] over already parsed (and possibly filtered) lines,
/// sorted by start time.
pub(crate) fn lines_to_ttml(lines: &[LrcLine], duration_ms: Option<u64>) -> Option<String> {
    let mut paragraphs = String::new();
    let mut first_begin = None;
    let mut last_end = 0;
    let mut key = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.text.is_empty() {
            continue;
        }
        let next = lines[index + 1..]
            .iter()
            .map(|l| l.start_ms)
            .find(|&start| start > line.start_ms);
        let end = next.unwrap_or_else(|| {
            let fallback = line.start_ms + LAST_LINE_MS;
            duration_ms
                .filter(|&d| d > line.start_ms)
                .map_or(fallback, |d| d.min(fallback))
        });
        key += 1;
        first_begin.get_or_insert(line.start_ms);
        last_end = last_end.max(end);
        // Writing into a String cannot fail.
        let _ = write!(
            paragraphs,
            "<p begin=\"{}\" end=\"{}\" itunes:key=\"L{key}\" ttm:agent=\"v1\">{}</p>",
            format_time(line.start_ms),
            format_time(end),
            escape_xml(&line.text)
        );
    }
    let first_begin = first_begin?;
    let body_dur = duration_ms.unwrap_or(last_end).max(last_end);
    Some(format!(
        "{TT_OPEN} itunes:timing=\"Line\"><head><metadata><ttm:agent type=\"person\" xml:id=\"v1\"/>\
         </metadata></head><body dur=\"{}\"><div begin=\"{}\" end=\"{}\">{paragraphs}</div></body></tt>",
        format_time(body_dur),
        format_time(first_begin),
        format_time(last_end),
    ))
}

/// How finely a TTML lyrics document is timed: Apple's `itunes:timing`
/// marker when present (`Word`, `Line`, `None`), otherwise `begin` timings
/// on `<span>`s (word) or only on `<p>`s (line), else plain.
pub(crate) fn ttml_quality(ttml: &str) -> LyricsQuality {
    if ttml.contains("itunes:timing=\"Word\"") {
        return LyricsQuality::Word;
    }
    if ttml.contains("itunes:timing=\"None\"") || !ttml.contains("begin=\"") {
        return LyricsQuality::Plain;
    }
    if ttml.contains("itunes:timing=\"Line\"") {
        return LyricsQuality::Line;
    }
    if ttml.contains("<span begin=\"") {
        LyricsQuality::Word
    } else {
        LyricsQuality::Line
    }
}

/// Wraps unsynced lyrics in TTML (`itunes:timing="None"`), one `<p>` per line.
///
/// Returns `None` when there is no non-empty line.
pub(crate) fn plain_to_ttml(text: &str) -> Option<String> {
    let mut paragraphs = String::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // Writing into a String cannot fail.
        let _ = write!(paragraphs, "<p>{}</p>", escape_xml(line));
    }
    (!paragraphs.is_empty()).then(|| {
        format!(
            "{TT_OPEN} itunes:timing=\"None\"><head><metadata/></head><body><div>{paragraphs}</div></body></tt>"
        )
    })
}

/// One timed word (or syllable) of a word-synced line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimedWord {
    /// Start in milliseconds from the track start.
    pub(crate) start_ms: u64,
    /// End in milliseconds from the track start.
    pub(crate) end_ms: u64,
    /// Text as the source has it; trailing whitespace separates it from
    /// the next word, none means the next syllable continues the word.
    pub(crate) text: String,
}

/// One word-synced line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimedLine {
    /// Line start in milliseconds.
    pub(crate) start_ms: u64,
    /// Line end in milliseconds.
    pub(crate) end_ms: u64,
    /// Its words, in order.
    pub(crate) words: Vec<TimedWord>,
}

impl TimedLine {
    /// The line's text, words joined as the source spaced them.
    pub(crate) fn text(&self) -> String {
        let joined: String = self.words.iter().map(|w| w.text.as_str()).collect();
        joined.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Drops empty words and glues punctuation-only tokens (`","` timed on
/// its own by Netease) onto the word before, as Apple writes `"Ooh,"`.
fn merge_punctuation(words: &[TimedWord]) -> Vec<TimedWord> {
    let mut out: Vec<TimedWord> = Vec::with_capacity(words.len());
    for word in words.iter().filter(|w| !w.text.trim().is_empty()) {
        let punctuation = !word.text.chars().any(char::is_alphanumeric);
        match out.last_mut() {
            Some(previous) if punctuation && !previous.text.ends_with(char::is_whitespace) => {
                previous.text.push_str(&word.text);
                previous.end_ms = previous.end_ms.max(word.end_ms);
            }
            _ => out.push(word.clone()),
        }
    }
    out
}

/// Converts word-synced lines into Apple-style word-timed TTML
/// (`itunes:timing="Word"`): one `<p begin end>` per line holding one
/// `<span begin end>` per word or syllable, with a space between words
/// (none between syllables of one word), exactly like Apple's own files.
///
/// Returns `None` when no line has a non-empty word.
pub(crate) fn words_to_ttml(lines: &[TimedLine], duration_ms: Option<u64>) -> Option<String> {
    let mut paragraphs = String::new();
    let mut first_begin = None;
    let mut last_end = 0;
    let mut key = 0;
    for line in lines {
        let merged = merge_punctuation(&line.words);
        let words: Vec<&TimedWord> = merged.iter().collect();
        let (Some(first), Some(last)) = (words.first(), words.last()) else {
            continue;
        };
        let begin = line.start_ms.min(first.start_ms);
        let end = line.end_ms.max(last.end_ms).max(begin);
        key += 1;
        first_begin.get_or_insert(begin);
        last_end = last_end.max(end);
        let mut spans = String::new();
        for (index, word) in words.iter().enumerate() {
            let text = word.text.trim();
            let word_end = word.end_ms.max(word.start_ms);
            // Writing into a String cannot fail.
            let _ = write!(
                spans,
                "<span begin=\"{}\" end=\"{}\">{}</span>",
                format_time(word.start_ms),
                format_time(word_end),
                escape_xml(text)
            );
            let spaced = word.text.ends_with(char::is_whitespace)
                || words
                    .get(index + 1)
                    .is_some_and(|next| next.text.starts_with(char::is_whitespace));
            if spaced && index + 1 < words.len() {
                spans.push(' ');
            }
        }
        let _ = write!(
            paragraphs,
            "<p begin=\"{}\" end=\"{}\" itunes:key=\"L{key}\" ttm:agent=\"v1\">{spans}</p>",
            format_time(begin),
            format_time(end),
        );
    }
    let first_begin = first_begin?;
    let body_dur = duration_ms.unwrap_or(last_end).max(last_end);
    Some(format!(
        "{TT_OPEN} itunes:timing=\"Word\"><head><metadata><ttm:agent type=\"person\" xml:id=\"v1\"/>\
         </metadata></head><body dur=\"{}\"><div begin=\"{}\" end=\"{}\">{paragraphs}</div></body></tt>",
        format_time(body_dur),
        format_time(first_begin),
        format_time(last_end),
    ))
}

/// Parses a TTML clock value (`1:02:03.5`, `02:03.250`, `3.25`, `3.25s`)
/// into milliseconds.
pub(crate) fn parse_clock(value: &str) -> Option<u64> {
    let value = value.trim().trim_end_matches('s');
    let mut parts = value.rsplit(':');
    let seconds = parts.next()?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut ms: u64 = whole.parse::<u64>().ok()? * 1_000;
    let digits: String = fraction.chars().chain("000".chars()).take(3).collect();
    ms += digits.parse::<u64>().ok()?;
    let mut scale = 60_000;
    for part in parts {
        ms += part.parse::<u64>().ok()? * scale;
        scale *= 60;
    }
    Some(ms)
}

/// Rewrites every `begin` / `end` / `dur` attribute into Apple's clock style
/// ([`format_time`]); values it cannot read stay as they are.
pub(crate) fn normalize_clock_attributes(ttml: &str) -> String {
    let mut out = String::with_capacity(ttml.len());
    let mut rest = ttml;
    loop {
        let next = ["begin=\"", "end=\"", "dur=\""]
            .iter()
            .filter_map(|attr| rest.find(attr).map(|at| (at, attr.len())))
            .min();
        let Some((at, len)) = next else {
            out.push_str(rest);
            return out;
        };
        // Only whole attribute names (" begin=" not "xbegin=").
        let boundary = at == 0 || rest[..at].ends_with(char::is_whitespace);
        let value_start = at + len;
        let Some(value_len) = rest[value_start..].find('"') else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..value_start]);
        let value = &rest[value_start..value_start + value_len];
        match parse_clock(value).filter(|_| boundary) {
            Some(ms) => out.push_str(&format_time(ms)),
            None => out.push_str(value),
        }
        rest = &rest[value_start + value_len..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(start_ms: u64, end_ms: u64, text: &str) -> TimedWord {
        TimedWord {
            start_ms,
            end_ms,
            text: text.to_owned(),
        }
    }

    fn sample_words() -> Vec<TimedLine> {
        vec![TimedLine {
            start_ms: 27_360,
            end_ms: 28_650,
            words: vec![
                word(27_360, 27_600, "I've "),
                word(27_600, 27_690, "been "),
                word(27_690, 28_050, "try"),
                word(28_050, 28_650, "na"),
            ],
        }]
    }

    #[test]
    fn word_ttml_should_be_word_quality() {
        let ttml = words_to_ttml(&sample_words(), None).unwrap();
        assert_eq!(ttml_quality(&ttml), LyricsQuality::Word);
    }

    #[test]
    fn word_ttml_should_space_words_but_not_syllables() {
        let ttml = words_to_ttml(&sample_words(), None).unwrap();
        assert!(ttml.contains(
            "<span begin=\"27.360\" end=\"27.600\">I've</span> <span begin=\"27.600\" end=\"27.690\">been</span> \
             <span begin=\"27.690\" end=\"28.050\">try</span><span begin=\"28.050\" end=\"28.650\">na</span></p>"
        ));
    }

    #[test]
    fn word_ttml_should_time_paragraphs_like_apple() {
        let ttml = words_to_ttml(&sample_words(), Some(200_000)).unwrap();
        assert!(ttml.contains(
            "<body dur=\"3:20.000\"><div begin=\"27.360\" end=\"28.650\"><p begin=\"27.360\" end=\"28.650\" itunes:key=\"L1\" ttm:agent=\"v1\">"
        ));
    }

    #[test]
    fn punctuation_token_should_join_the_word_before() {
        let lines = [TimedLine {
            start_ms: 16_640,
            end_ms: 17_660,
            words: vec![
                word(16_640, 17_060, "Ooh"),
                word(17_060, 17_120, ", "),
                word(17_120, 17_660, "girl"),
            ],
        }];
        let ttml = words_to_ttml(&lines, None).unwrap();
        assert!(ttml.contains(
            "<span begin=\"16.640\" end=\"17.120\">Ooh,</span> <span begin=\"17.120\" end=\"17.660\">girl</span>"
        ));
    }

    #[test]
    fn word_ttml_without_words_should_be_none() {
        let lines = [TimedLine {
            start_ms: 0,
            end_ms: 1,
            words: vec![word(0, 1, " ")],
        }];
        assert_eq!(words_to_ttml(&lines, None), None);
    }

    #[test]
    fn clock_should_parse_minutes_and_fractions() {
        assert_eq!(
            [
                parse_clock("00:00.917"),
                parse_clock("1:02:03.5"),
                parse_clock("3.25s")
            ],
            [Some(917), Some(3_723_500), Some(3_250)]
        );
    }

    #[test]
    fn clock_attributes_should_use_apple_style() {
        assert_eq!(
            normalize_clock_attributes(
                r#"<body dur="03:01.128"><p begin="00:00.917" end="00:03.669">"#
            ),
            r#"<body dur="3:01.128"><p begin="0.917" end="3.669">"#
        );
    }

    #[test]
    fn word_timing_marker_should_be_word_quality() {
        let ttml = r#"<tt itunes:timing="Word"><body><p begin="1"><span begin="1">a</span></p></body></tt>"#;
        assert_eq!(ttml_quality(ttml), LyricsQuality::Word);
    }

    #[test]
    fn spans_without_marker_should_be_word_quality() {
        let ttml = r#"<tt><body><p begin="1"><span begin="1">a</span></p></body></tt>"#;
        assert_eq!(ttml_quality(ttml), LyricsQuality::Word);
    }

    #[test]
    fn converted_lrc_should_be_line_quality() {
        let ttml = lrc_to_ttml(
            "[00:01.00]a
[00:02.00]b",
            None,
        )
        .unwrap();
        assert_eq!(ttml_quality(&ttml), LyricsQuality::Line);
    }

    #[test]
    fn plain_text_should_be_plain_quality() {
        let ttml = plain_to_ttml(
            "a
b",
        )
        .unwrap();
        assert_eq!(ttml_quality(&ttml), LyricsQuality::Plain);
    }

    #[test]
    fn timestamp_with_centiseconds_should_parse() {
        assert_eq!(parse_timestamp("01:02.34"), Some(62_340));
    }

    #[test]
    fn metadata_tag_should_not_parse_as_time() {
        assert_eq!(parse_timestamp("ar:Someone"), None);
    }

    #[test]
    fn multiple_stamps_should_repeat_line() {
        let lines = parse_lrc("[00:10.00][00:30.00]Chorus\n[00:20.00]Verse");
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Chorus", "Verse", "Chorus"]);
    }

    #[test]
    fn offset_should_shift_earlier() {
        let lines = parse_lrc("[offset:+500]\n[00:10.00]A");
        assert_eq!(lines[0].start_ms, 9_500);
    }

    #[test]
    fn word_tags_should_be_stripped() {
        let lines = parse_lrc("[00:01.00]<00:01.00>Hello <00:01.50>world");
        assert_eq!(lines[0].text, "Hello world");
    }

    #[test]
    fn format_time_should_match_apple_style() {
        assert_eq!(
            [
                format_time(17_800),
                format_time(251_250),
                format_time(3_723_000)
            ],
            ["17.800", "4:11.250", "1:02:03.000"]
        );
    }

    #[test]
    fn line_should_end_at_next_stamp() {
        let ttml = lrc_to_ttml("[00:01.00]A\n[00:03.50]\n[00:05.00]B", None).unwrap();
        assert!(ttml.contains("<p begin=\"1.000\" end=\"3.500\" itunes:key=\"L1\""));
    }

    #[test]
    fn last_line_should_get_default_duration() {
        let ttml = lrc_to_ttml("[00:01.00]A", None).unwrap();
        assert!(ttml.contains("begin=\"1.000\" end=\"6.000\""));
    }

    #[test]
    fn text_should_be_escaped() {
        let ttml = lrc_to_ttml("[00:01.00]Rock & <Roll>", None).unwrap();
        assert!(ttml.contains("Rock &amp; &lt;Roll&gt;"));
    }

    #[test]
    fn lrc_without_lines_should_yield_none() {
        assert_eq!(lrc_to_ttml("[ar:x]\nno stamps", None), None);
    }

    #[test]
    fn plain_ttml_should_not_count_as_synced() {
        let ttml = plain_to_ttml(
            "one
two",
        )
        .unwrap();
        assert!(!ttml_quality(&ttml).is_synced());
    }

    #[test]
    fn line_ttml_should_count_as_synced() {
        let ttml = lrc_to_ttml(
            "[00:01.00]one
[00:02.00]two",
            None,
        )
        .unwrap();
        assert!(ttml_quality(&ttml).is_synced());
    }

    #[test]
    fn ttml_should_declare_line_timing() {
        let ttml = lrc_to_ttml("[00:01.00]A", Some(10_000)).unwrap();
        assert!(ttml.starts_with("<tt ") && ttml.contains("itunes:timing=\"Line\""));
    }

    #[test]
    fn plain_should_wrap_lines() {
        assert!(
            plain_to_ttml("a\n\nb")
                .unwrap()
                .contains("<div><p>a</p><p>b</p></div>")
        );
    }
}
