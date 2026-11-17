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

#[cfg(test)]
mod tests {
    use super::*;

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
}
