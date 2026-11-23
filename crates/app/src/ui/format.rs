//! Display formatting helpers.

use std::fmt::Write as _;

use cider_offline_core::storage::{LyricsQuality, LyricsSource};
use cider_offline_core::{LyricsProgress, LyricsReport};

/// Formats a duration as `m:ss` (or `h:mm:ss`); empty when unknown.
pub fn duration(ms: Option<u32>) -> String {
    let Some(ms) = ms else {
        return String::new();
    };
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Formats a counter with thin thousands separators (`12 345`).
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

/// `"1 song"` / `"12 songs"` style count with thousands separators.
fn counted(value: usize, one: &str, many: &str) -> String {
    let noun = if value == 1 { one } else { many };
    format!("{} {noun}", count(u64::try_from(value).unwrap_or(u64::MAX)))
}

/// The Library header count line: "248 albums and playlists, 3 120 songs".
pub fn library_counts(collections: usize, songs: usize) -> String {
    format!(
        "{}, {}",
        counted(collections, "album or playlist", "albums and playlists"),
        counted(songs, "song", "songs")
    )
}

/// The collection detail meta line: "14 songs, 61 minutes" (hours from 60
/// minutes on), with ", Custom" for user-created collections.
pub fn collection_meta(songs: usize, total_ms: u64, custom: bool) -> String {
    let minutes = usize::try_from((total_ms + 30_000) / 60_000).unwrap_or(usize::MAX);
    let mut meta = counted(songs, "song", "songs");
    if minutes >= 60 {
        meta.push_str(", ");
        meta.push_str(&counted(minutes / 60, "hour", "hours"));
        if minutes % 60 > 0 {
            meta.push_str(", ");
            meta.push_str(&counted(minutes % 60, "minute", "minutes"));
        }
    } else if minutes > 0 {
        meta.push_str(", ");
        meta.push_str(&counted(minutes, "minute", "minutes"));
    }
    if custom {
        meta.push_str(", Custom");
    }
    meta
}

/// UI key for a lyrics source (`LibraryState` badge text).
pub fn lyrics(source: LyricsSource) -> &'static str {
    match source {
        LyricsSource::Apple => "apple",
        LyricsSource::Lrclib => "lrclib",
        LyricsSource::Amll => "amll",
        LyricsSource::Netease => "netease",
        LyricsSource::Kugou => "kugou",
        LyricsSource::Musixmatch => "musixmatch",
        LyricsSource::Community => "community",
        LyricsSource::Custom => "custom",
        LyricsSource::None => "none",
    }
}

/// UI code of the lyrics timing (`TrackRow.lyrics-quality`): 0 none,
/// 1 plain, 2 line-synced, 3 word-synced. No lyrics source means none.
pub fn lyrics_quality(source: LyricsSource, quality: LyricsQuality) -> i32 {
    if source == LyricsSource::None {
        return 0;
    }
    match quality {
        LyricsQuality::None | LyricsQuality::Plain => 1,
        LyricsQuality::Line => 2,
        LyricsQuality::Word => 3,
    }
}

/// "40 not on LRCLIB" or "40 without lyrics".
fn missing_text(missing: usize, lrclib_only: bool) -> String {
    if lrclib_only {
        format!("{missing} not on LRCLIB")
    } else {
        format!("{missing} without lyrics")
    }
}

/// Toast text while a bulk lyrics lookup runs.
pub fn lyrics_progress(p: &LyricsProgress, lrclib_only: bool) -> String {
    let mut text = if p.pass == 0 {
        format!(
            "Lyrics: {} of {} checked, {} found",
            p.checked, p.total, p.found
        )
    } else {
        format!("Lyrics: {} found", p.found)
    };
    if p.missing > 0 {
        let _ = write!(text, ", {}", missing_text(p.missing, lrclib_only));
    }
    if p.deferred > 0 {
        let _ = write!(text, ", {} deferred", p.deferred);
    }
    if p.failed > 0 {
        let _ = write!(text, ", {} failed", p.failed);
    }
    if p.pass > 0 {
        let _ = write!(
            text,
            ", retrying deferred (pass {} of {})",
            p.pass, p.passes
        );
        if let Some(wait) = p.waiting {
            let _ = write!(text, " in {} s", wait.as_secs());
        }
    }
    text.push('…');
    text
}

/// Toast text when a bulk lyrics lookup finished.
pub fn lyrics_report(r: &LyricsReport, lrclib_only: bool) -> String {
    let mut text = format!("Lyrics: {} found", r.found);
    let mut kinds = Vec::new();
    for (count, label) in [
        (r.word, "word-synced"),
        (r.line, "line-synced"),
        (r.plain, "plain"),
    ] {
        if count > 0 {
            kinds.push(format!("{count} {label}"));
        }
    }
    if !kinds.is_empty() {
        let _ = write!(text, " ({} new)", kinds.join(", "));
    }
    if r.missing > 0 {
        let _ = write!(text, ", {}", missing_text(r.missing, lrclib_only));
    }
    if r.failed > 0 {
        let _ = write!(text, ", {} failed (try again later)", r.failed);
    }
    if r.passes > 0 {
        let _ = write!(
            text,
            ". {} recovered in {} retry pass{}",
            r.recovered,
            r.passes,
            if r.passes == 1 { "" } else { "es" }
        );
    }
    text.push('.');
    text
}

/// A file size for people: "512 KB", "24.1 MB", "1.3 GB".
#[expect(clippy::cast_precision_loss, reason = "a rounded, display-only size")]
pub fn bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    let value = bytes as f64;
    if value < KB * KB {
        format!(
            "{:.0} KB",
            (value / KB).max(if bytes > 0 { 1.0 } else { 0.0 })
        )
    } else if value < KB * KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else {
        format!("{:.1} GB", value / (KB * KB * KB))
    }
}

/// "3 songs, 24.1 MB" (a count of downloaded files and their size).
pub fn files_and_size(files: usize, size: u64) -> String {
    let noun = if files == 1 { "song" } else { "songs" };
    format!("{files} {noun}, {}", bytes(size))
}

/// The most songs a dialog or message names; more are counted.
pub const MAX_NAMED_SONGS: usize = 5;

/// `count` songs for a dialog or message: their names ("“A”", "“A” and
/// “B”", "“A”, “B” and “C”") when there are at most [`MAX_NAMED_SONGS`]
/// and `titles` names each of them, else a count ("37 songs").
pub fn songs(count: usize, titles: &[String]) -> String {
    let named = count <= MAX_NAMED_SONGS
        && titles.len() == count
        && titles.iter().all(|title| !title.trim().is_empty());
    if count == 0 || !named {
        return counted(count, "song", "songs");
    }
    let mut text = String::new();
    for (i, title) in titles.iter().enumerate() {
        if i > 0 {
            text.push_str(if i + 1 == count { " and " } else { ", " });
        }
        let _ = write!(text, "“{}”", title.trim());
    }
    text
}

/// "is" for one song, "are" for more.
pub fn is_are(count: usize) -> &'static str {
    if count == 1 { "is" } else { "are" }
}

/// `text` with an invisible line-break opportunity (U+200B) after every
/// path separator, so a long path in a wrapped label breaks between folders
/// instead of forcing the label (and the page) wider than the window.
pub fn breakable(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for c in text.chars() {
        out.push(c);
        if matches!(c, '\\' | '/') {
            out.push('\u{200B}');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(names: &[&str]) -> Vec<String> {
        names.iter().map(|&name| name.to_owned()).collect()
    }

    #[test]
    fn songs_should_name_up_to_five_songs() {
        assert_eq!(
            [
                songs(1, &titles(&["One"])),
                songs(2, &titles(&["One", "Two"])),
                songs(5, &titles(&["A", "B", "C", "D", "E"])),
            ],
            [
                "“One”".to_owned(),
                "“One” and “Two”".to_owned(),
                "“A”, “B”, “C”, “D” and “E”".to_owned(),
            ]
        );
    }

    #[test]
    fn songs_should_count_more_than_five_songs() {
        assert_eq!(
            songs(6, &titles(&["A", "B", "C", "D", "E", "F"])),
            "6 songs"
        );
    }

    #[test]
    fn songs_should_count_when_a_title_is_missing() {
        assert_eq!(
            [songs(2, &titles(&["One"])), songs(1, &titles(&[" "]))],
            ["2 songs".to_owned(), "1 song".to_owned()]
        );
    }

    #[test]
    fn bytes_should_pick_a_readable_unit() {
        assert_eq!(
            [
                bytes(0),
                bytes(700),
                bytes(24_100_000),
                bytes(1_300_000_000)
            ],
            ["0 KB", "1 KB", "24.1 MB", "1.3 GB"]
        );
    }

    #[test]
    fn files_and_size_should_count_songs() {
        assert_eq!(files_and_size(1, 5_000_000), "1 song, 5.0 MB");
    }

    #[test]
    fn progress_should_mention_retry_pass() {
        let p = LyricsProgress {
            found: 120,
            missing: 40,
            deferred: 15,
            pass: 1,
            passes: 3,
            ..LyricsProgress::default()
        };
        assert_eq!(
            lyrics_progress(&p, true),
            "Lyrics: 120 found, 40 not on LRCLIB, 15 deferred, retrying deferred (pass 1 of 3)…"
        );
    }

    #[test]
    fn report_should_count_qualities() {
        let r = LyricsReport {
            found: 3,
            word: 1,
            line: 2,
            missing: 1,
            ..LyricsReport::default()
        };
        assert_eq!(
            lyrics_report(&r, false),
            "Lyrics: 3 found (1 word-synced, 2 line-synced new), 1 without lyrics."
        );
    }

    #[test]
    fn word_synced_lyrics_should_map_to_three() {
        assert_eq!(lyrics_quality(LyricsSource::Apple, LyricsQuality::Word), 3);
    }

    #[test]
    fn line_synced_lyrics_should_map_to_two() {
        assert_eq!(lyrics_quality(LyricsSource::Lrclib, LyricsQuality::Line), 2);
    }

    #[test]
    fn lyrics_without_timing_should_map_to_plain() {
        assert_eq!(lyrics_quality(LyricsSource::Lrclib, LyricsQuality::None), 1);
    }

    #[test]
    fn every_lyrics_source_should_map_to_its_own_key() {
        assert_eq!(lyrics(LyricsSource::Netease), "netease");
        assert_eq!(lyrics(LyricsSource::Amll), "amll");
        assert_eq!(lyrics(LyricsSource::Kugou), "kugou");
    }

    #[test]
    fn missing_lyrics_should_map_to_none() {
        assert_eq!(lyrics_quality(LyricsSource::None, LyricsQuality::Word), 0);
    }

    #[test]
    fn duration_should_format_minutes_and_seconds() {
        assert_eq!(duration(Some(185_400)), "3:05");
    }

    #[test]
    fn duration_should_include_hours_when_long() {
        assert_eq!(duration(Some(3_725_000)), "1:02:05");
    }

    #[test]
    fn duration_should_be_empty_when_unknown() {
        assert_eq!(duration(None), "");
    }

    #[test]
    fn count_should_group_thousands() {
        assert_eq!(count(1_234_567), "1\u{202f}234\u{202f}567");
    }

    #[test]
    fn collection_meta_should_count_songs_and_minutes() {
        assert_eq!(
            collection_meta(14, 55 * 60_000 - 20_000, false),
            "14 songs, 55 minutes"
        );
    }

    #[test]
    fn collection_meta_should_switch_to_hours() {
        assert_eq!(
            collection_meta(315, (17 * 60 + 5) * 60_000, false),
            "315 songs, 17 hours, 5 minutes"
        );
    }

    #[test]
    fn collection_meta_should_mark_custom_and_singular() {
        assert_eq!(collection_meta(1, 60_000, true), "1 song, 1 minute, Custom");
    }

    #[test]
    fn library_counts_should_use_singular_and_plural() {
        assert_eq!(library_counts(1, 2), "1 album or playlist, 2 songs");
    }

    #[test]
    fn count_should_leave_small_numbers_alone() {
        assert_eq!(count(999), "999");
    }

    #[test]
    fn breakable_should_allow_a_break_after_each_path_separator() {
        assert_eq!(
            breakable(r"C:\Users\a/b"),
            "C:\\\u{200B}Users\\\u{200B}a/\u{200B}b"
        );
    }

    #[test]
    fn breakable_should_leave_text_without_separators_alone() {
        assert_eq!(breakable("No path here."), "No path here.");
    }
}
