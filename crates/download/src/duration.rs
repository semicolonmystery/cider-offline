//! Writes the real playback length into a decrypted fragmented MP4.
//!
//! Apple's streams declare a duration of 0 in `mvhd`, `tkhd` and `mdhd` and
//! carry no `mvex/mehd`, because a streaming player learns the length from
//! the playlist. A saved file then looks a few seconds long to any player
//! that cannot scan every fragment first (ffprobe on a pipe reports the first
//! fragment only, about 10 s). [`write_duration`] sums the sample durations of
//! every fragment and writes the total into those three headers, plus a
//! `mehd` (the fragment duration box ISO/IEC 14496-12 §8.8.2 defines for
//! exactly this).
//!
//! The header fields are rewritten in place. Adding `mehd` grows `mvex` and
//! `moov` by its size, which moves every byte after it, so the absolute file
//! offsets that point past the insertion point are shifted to match:
//! `tfhd.base_data_offset`, `stco` / `co64` chunk offsets and `mfra/tfra`
//! fragment offsets. Offsets relative to a `moof` (Apple's
//! `default-base-is-moof` layout) need no change.

use crate::cenc::{
    BoxRef, find_box, find_box_path, full_box_header, parse_tfhd, parse_trun, read_box, read_trex,
};
use crate::error::DrmError;

/// Size of a version-0 `mehd` box: header (8) + version/flags (4) + u32.
const MEHD_V0_SIZE: u32 = 16;
/// Size of a version-1 `mehd` box: header (8) + version/flags (4) + u64.
const MEHD_V1_SIZE: u32 = 20;

fn err(message: impl Into<String>) -> DrmError {
    DrmError::Decrypt(format!("duration: {}", message.into()))
}

/// A duration field inside a header box.
#[derive(Debug, Clone, Copy)]
struct DurationField {
    /// Buffer offset of the field.
    at: usize,
    /// `true` for a 64-bit field (box version 1), `false` for 32-bit.
    wide: bool,
}

/// What a `trak` needs for the duration fix.
#[derive(Debug, Clone, Copy)]
struct TrackHeaders {
    track_id: u32,
    tkhd: DurationField,
    mdhd: DurationField,
    media_timescale: u32,
}

/// Reads a big-endian unsigned integer of 4 or 8 bytes.
fn read_uint(buf: &[u8], at: usize, wide: bool) -> Result<u64, DrmError> {
    let width = if wide { 8 } else { 4 };
    let bytes = buf
        .get(at..at + width)
        .ok_or_else(|| err("field past the end of the buffer"))?;
    Ok(bytes
        .iter()
        .fold(0u64, |acc, &byte| (acc << 8) | u64::from(byte)))
}

/// Writes `value` into a 4- or 8-byte big-endian field.
fn write_uint(buf: &mut [u8], field: DurationField, value: u64) -> Result<(), DrmError> {
    if field.wide {
        buf.get_mut(field.at..field.at + 8)
            .ok_or_else(|| err("field past the end of the buffer"))?
            .copy_from_slice(&value.to_be_bytes());
    } else {
        let narrow = u32::try_from(value)
            .map_err(|_| err(format!("{value} does not fit a version-0 field")))?;
        buf.get_mut(field.at..field.at + 4)
            .ok_or_else(|| err("field past the end of the buffer"))?
            .copy_from_slice(&narrow.to_be_bytes());
    }
    Ok(())
}

/// `(timescale, duration field)` of an `mvhd` or `mdhd` box; both start with
/// creation time, modification time, timescale and duration.
fn timescale_and_duration(buf: &[u8], bref: BoxRef) -> Result<(u32, DurationField), DrmError> {
    let (version, _, body) =
        full_box_header(buf, bref).ok_or_else(|| err("truncated full-box header"))?;
    let wide = version == 1;
    let timescale_at = body + if wide { 16 } else { 8 };
    let duration_at = timescale_at + 4;
    if duration_at + if wide { 8 } else { 4 } > bref.end() {
        return Err(err("truncated mvhd / mdhd"));
    }
    let timescale = u32::try_from(read_uint(buf, timescale_at, false)?)
        .map_err(|_| err("timescale overflow"))?;
    Ok((
        timescale,
        DurationField {
            at: duration_at,
            wide,
        },
    ))
}

/// `(track_ID, duration field)` of a `tkhd` box.
fn tkhd_fields(buf: &[u8], bref: BoxRef) -> Result<(u32, DurationField), DrmError> {
    let (version, _, body) =
        full_box_header(buf, bref).ok_or_else(|| err("tkhd: truncated full-box header"))?;
    let wide = version == 1;
    let track_id_at = body + if wide { 16 } else { 8 };
    // track_ID (4) + reserved (4), then duration.
    let duration_at = track_id_at + 8;
    if duration_at + if wide { 8 } else { 4 } > bref.end() {
        return Err(err("truncated tkhd"));
    }
    let track_id =
        u32::try_from(read_uint(buf, track_id_at, false)?).map_err(|_| err("track id overflow"))?;
    Ok((
        track_id,
        DurationField {
            at: duration_at,
            wide,
        },
    ))
}

fn track_headers(buf: &[u8], trak: BoxRef) -> Result<TrackHeaders, DrmError> {
    let tkhd = find_box(buf, trak.payload, trak.end(), *b"tkhd")?
        .ok_or_else(|| err("trak without tkhd"))?;
    let (track_id, tkhd) = tkhd_fields(buf, tkhd)?;
    let mdhd = find_box_path(buf, trak.payload, trak.end(), &[*b"mdia", *b"mdhd"])?
        .ok_or_else(|| err("trak without mdia/mdhd"))?;
    let (media_timescale, mdhd) = timescale_and_duration(buf, mdhd)?;
    if media_timescale == 0 {
        return Err(err(format!("track {track_id} has timescale 0")));
    }
    Ok(TrackHeaders {
        track_id,
        tkhd,
        mdhd,
        media_timescale,
    })
}

/// Sum of every fragment's sample durations, per `track_ID`, in media
/// timescale units.
fn fragment_durations(buf: &[u8]) -> Result<Vec<(u32, u64)>, DrmError> {
    let trex = read_trex(buf)?;
    let mut totals: Vec<(u32, u64)> = Vec::new();
    let mut pos = 0;
    while let Some((t, moof)) = read_box(buf, pos)? {
        pos = moof.end();
        if t != *b"moof" {
            continue;
        }
        let mut traf_pos = moof.payload;
        while traf_pos < moof.end() {
            let Some((t, traf)) = read_box(buf, traf_pos)? else {
                break;
            };
            traf_pos = traf.end();
            if t == *b"traf" {
                let (track_id, ticks) = traf_duration(buf, traf, &trex)?;
                match totals.iter_mut().find(|(id, _)| *id == track_id) {
                    Some((_, total)) => *total += ticks,
                    None => totals.push((track_id, ticks)),
                }
            }
        }
    }
    Ok(totals)
}

/// `(track_ID, duration)` of one `traf`: per-sample `trun` durations, else
/// the `tfhd` default, else the `trex` default.
fn traf_duration(
    buf: &[u8],
    traf: BoxRef,
    trex: &[crate::cenc::TrackExtends],
) -> Result<(u32, u64), DrmError> {
    let tfhd = find_box(buf, traf.payload, traf.end(), *b"tfhd")?
        .ok_or_else(|| err("traf without tfhd"))?;
    let tfhd = parse_tfhd(buf, tfhd)?;
    let default = tfhd
        .default_sample_duration
        .or_else(|| {
            trex.iter()
                .find(|t| t.track_id == tfhd.track_id)
                .map(|t| t.default_sample_duration)
        })
        .filter(|&d| d > 0);
    let mut ticks = 0u64;
    let mut pos = traf.payload;
    while pos < traf.end() {
        let Some((t, b)) = read_box(buf, pos)? else {
            break;
        };
        pos = b.end();
        if t == *b"trun" {
            let run = parse_trun(buf, b, tfhd.default_sample_size)?;
            ticks += if let Some(durations) = &run.durations {
                durations.iter().map(|&d| u64::from(d)).sum()
            } else {
                let each = default.ok_or_else(|| {
                    err(format!(
                        "track {} has samples without a duration",
                        tfhd.track_id
                    ))
                })?;
                u64::from(each) * run.sample_count() as u64
            };
        }
    }
    Ok((tfhd.track_id, ticks))
}

/// `value * to / from`, rounded to nearest, without overflow.
fn rescale(value: u64, from: u32, to: u32) -> u64 {
    let scaled = (u128::from(value) * u128::from(to) + u128::from(from) / 2) / u128::from(from);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Adds `delta` to every absolute file offset at or past `from`:
/// `tfhd.base_data_offset`, `stco` / `co64` entries and `mfra/tfra` moof
/// offsets. Runs on the buffer before the insertion, so field positions are
/// still the original ones.
fn shift_absolute_offsets(buf: &mut [u8], from: usize, delta: usize) -> Result<(), DrmError> {
    let threshold = from as u64;
    let delta = delta as u64;
    let shift = |value: u64| {
        if value >= threshold {
            value + delta
        } else {
            value
        }
    };

    let mut fields: Vec<DurationField> = Vec::new();
    let mut pos = 0;
    while let Some((t, top)) = read_box(buf, pos)? {
        pos = top.end();
        match &t {
            b"moov" => collect_chunk_offsets(buf, top, &mut fields)?,
            b"moof" => {
                let mut p = top.payload;
                while p < top.end() {
                    let Some((tt, traf)) = read_box(buf, p)? else {
                        break;
                    };
                    p = traf.end();
                    if tt != *b"traf" {
                        continue;
                    }
                    if let Some(tfhd) = find_box(buf, traf.payload, traf.end(), *b"tfhd")?
                        && let Some(at) = parse_tfhd(buf, tfhd)?.base_data_offset_at
                    {
                        fields.push(DurationField { at, wide: true });
                    }
                }
            }
            b"mfra" => collect_tfra_offsets(buf, top, &mut fields)?,
            b"sidx" if top.offset < from => {
                return Err(err("a sidx before moov references moved data"));
            }
            _ => {}
        }
    }
    for field in fields {
        let value = read_uint(buf, field.at, field.wide)?;
        write_uint(buf, field, shift(value))?;
    }
    Ok(())
}

/// Every `stco` / `co64` entry inside `moov`.
fn collect_chunk_offsets(
    buf: &[u8],
    moov: BoxRef,
    fields: &mut Vec<DurationField>,
) -> Result<(), DrmError> {
    let mut pos = moov.payload;
    while pos < moov.end() {
        let Some((t, trak)) = read_box(buf, pos)? else {
            break;
        };
        pos = trak.end();
        if t != *b"trak" {
            continue;
        }
        let Some(stbl) = find_box_path(
            buf,
            trak.payload,
            trak.end(),
            &[*b"mdia", *b"minf", *b"stbl"],
        )?
        else {
            continue;
        };
        for (kind, wide) in [(*b"stco", false), (*b"co64", true)] {
            let Some(table) = find_box(buf, stbl.payload, stbl.end(), kind)? else {
                continue;
            };
            let (_, _, body) =
                full_box_header(buf, table).ok_or_else(|| err("truncated chunk offset box"))?;
            let count = usize::try_from(read_uint(buf, body, false)?)
                .map_err(|_| err("chunk count overflow"))?;
            let width = if wide { 8 } else { 4 };
            if body + 4 + count * width > table.end() {
                return Err(err("truncated chunk offset table"));
            }
            fields.extend((0..count).map(|i| DurationField {
                at: body + 4 + i * width,
                wide,
            }));
        }
    }
    Ok(())
}

/// Every `moof_offset` of every `tfra` inside `mfra`.
fn collect_tfra_offsets(
    buf: &[u8],
    mfra: BoxRef,
    fields: &mut Vec<DurationField>,
) -> Result<(), DrmError> {
    let mut pos = mfra.payload;
    while pos < mfra.end() {
        let Some((t, tfra)) = read_box(buf, pos)? else {
            break;
        };
        pos = tfra.end();
        if t != *b"tfra" {
            continue;
        }
        let (version, _, body) =
            full_box_header(buf, tfra).ok_or_else(|| err("tfra: truncated header"))?;
        let sizes = read_uint(buf, body + 4, false)?;
        let count = usize::try_from(read_uint(buf, body + 8, false)?)
            .map_err(|_| err("tfra count overflow"))?;
        let wide = version == 1;
        let time_width = if wide { 8 } else { 4 };
        // traf_number, trun_number and sample_number widths (1..=4 bytes).
        let tail: usize = [(sizes >> 4) & 3, (sizes >> 2) & 3, sizes & 3]
            .iter()
            .map(|&n| usize::try_from(n).unwrap_or(0) + 1)
            .sum();
        let entry = 2 * time_width + tail;
        let entries = body + 12;
        if entries + count * entry > tfra.end() {
            return Err(err("truncated tfra"));
        }
        fields.extend((0..count).map(|i| DurationField {
            at: entries + i * entry + time_width,
            wide,
        }));
    }
    Ok(())
}

/// Grows the size field of the box at `bref` by `delta`.
fn grow_box(buf: &mut [u8], bref: BoxRef, delta: usize) -> Result<(), DrmError> {
    let raw = read_uint(buf, bref.offset, false)?;
    match raw {
        // Size 0 ("to the end of the file") stays correct.
        0 => Ok(()),
        1 => {
            let field = DurationField {
                at: bref.offset + 8,
                wide: true,
            };
            let size = read_uint(buf, field.at, true)?;
            write_uint(buf, field, size + delta as u64)
        }
        size => write_uint(
            buf,
            DurationField {
                at: bref.offset,
                wide: false,
            },
            size + delta as u64,
        ),
    }
}

/// Writes the summed fragment duration into `mvhd`, every `tkhd` and `mdhd`,
/// and `mvex/mehd` (added when missing), and returns the length in
/// milliseconds.
///
/// A file that already declares the right durations is returned unchanged
/// (apart from an existing `mehd` being rewritten in place), so the step is
/// idempotent.
///
/// # Errors
///
/// [`DrmError::Decrypt`] when the file has no `moov/mvhd`, a track lacks its
/// headers or a timescale, a sample has no duration, a value does not fit
/// its field, or a box is malformed. Everything is checked before the
/// first write except the header values themselves, so a failed call leaves
/// a playable file (at worst with some correct header durations written).
pub(crate) fn write_duration(buf: &mut Vec<u8>) -> Result<u32, DrmError> {
    let moov = find_box(buf, 0, buf.len(), *b"moov")?.ok_or_else(|| err("no moov"))?;
    let mvhd = find_box(buf, moov.payload, moov.end(), *b"mvhd")?
        .ok_or_else(|| err("moov without mvhd"))?;
    let (movie_timescale, mvhd_duration) = timescale_and_duration(buf, mvhd)?;
    if movie_timescale == 0 {
        return Err(err("movie timescale 0"));
    }

    let mut tracks = Vec::new();
    let mut pos = moov.payload;
    while pos < moov.end() {
        let Some((t, b)) = read_box(buf, pos)? else {
            break;
        };
        pos = b.end();
        if t == *b"trak" {
            tracks.push(track_headers(buf, b)?);
        }
    }
    let totals = fragment_durations(buf)?;

    let mut movie_duration = 0u64;
    for track in &tracks {
        let media = totals
            .iter()
            .find(|(id, _)| *id == track.track_id)
            .map_or(0, |(_, ticks)| *ticks);
        let in_movie = rescale(media, track.media_timescale, movie_timescale);
        write_uint(buf, track.mdhd, media)?;
        write_uint(buf, track.tkhd, in_movie)?;
        movie_duration = movie_duration.max(in_movie);
    }
    if movie_duration == 0 {
        return Err(err("no fragment carries any samples"));
    }
    write_uint(buf, mvhd_duration, movie_duration)?;

    if let Some(mvex) = find_box(buf, moov.payload, moov.end(), *b"mvex")? {
        if let Some(mehd) = find_box(buf, mvex.payload, mvex.end(), *b"mehd")? {
            let (version, _, body) =
                full_box_header(buf, mehd).ok_or_else(|| err("mehd: truncated header"))?;
            let field = DurationField {
                at: body,
                wide: version == 1,
            };
            write_uint(buf, field, movie_duration)?;
        } else {
            let fragment_box = mehd_box(movie_duration);
            let at = mvex.payload;
            shift_absolute_offsets(buf, at, fragment_box.len())?;
            grow_box(buf, mvex, fragment_box.len())?;
            grow_box(buf, moov, fragment_box.len())?;
            buf.splice(at..at, fragment_box);
        }
    }

    let millis = rescale(movie_duration, movie_timescale, 1_000);
    u32::try_from(millis).map_err(|_| err("duration exceeds u32 milliseconds"))
}

/// A `mehd` box holding `fragment_duration` (version 0 when it fits 32 bits).
fn mehd_box(fragment_duration: u64) -> Vec<u8> {
    let (size, version, value) = match u32::try_from(fragment_duration) {
        Ok(narrow) => (MEHD_V0_SIZE, 0u8, narrow.to_be_bytes().to_vec()),
        Err(_) => (MEHD_V1_SIZE, 1u8, fragment_duration.to_be_bytes().to_vec()),
    };
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"mehd");
    b.extend_from_slice(&[version, 0, 0, 0]);
    b.extend_from_slice(&value);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_box(ty: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + payload.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(payload);
        v
    }

    fn mk_full_box(ty: &[u8], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut body = flags.to_be_bytes().to_vec();
        body[0] = version;
        body.extend_from_slice(payload);
        mk_box(ty, &body)
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// The fixed file and its milliseconds.
    fn fixed(mut file: Vec<u8>) -> (Vec<u8>, u32) {
        let millis = write_duration(&mut file).unwrap();
        (file, millis)
    }

    const MOVIE_TS: u32 = 1_000;
    const MEDIA_TS: u32 = 48_000;
    const FRAME: u32 = 1_024;

    /// `ftyp` + `moov` (mvhd, one audio trak with a `stco` of `chunk`, mvex
    /// with a trex defaulting to [`FRAME`]), all durations 0.
    fn mk_init(chunk_offset: Option<u32>) -> Vec<u8> {
        let mvhd = mk_full_box(
            b"mvhd",
            0,
            0,
            &cat(&[&[0; 8], &MOVIE_TS.to_be_bytes(), &[0; 4], &[0; 80]]),
        );
        let tkhd = mk_full_box(
            b"tkhd",
            0,
            7,
            &cat(&[&[0; 8], &1u32.to_be_bytes(), &[0; 4], &[0; 4], &[0; 60]]),
        );
        let media_header = mk_full_box(
            b"mdhd",
            0,
            0,
            &cat(&[
                &[0; 8],
                &MEDIA_TS.to_be_bytes(),
                &[0; 4],
                &[0x55, 0xC4, 0, 0],
            ]),
        );
        let stco = match chunk_offset {
            Some(offset) => mk_full_box(
                b"stco",
                0,
                0,
                &cat(&[&1u32.to_be_bytes(), &offset.to_be_bytes()]),
            ),
            None => mk_full_box(b"stco", 0, 0, &0u32.to_be_bytes()),
        };
        let stbl = mk_box(b"stbl", &stco);
        let minf = mk_box(b"minf", &stbl);
        let mdia = mk_box(b"mdia", &cat(&[&media_header, &minf]));
        let trak = mk_box(b"trak", &cat(&[&tkhd, &mdia]));
        let trex = mk_full_box(
            b"trex",
            0,
            0,
            &cat(&[
                &1u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &FRAME.to_be_bytes(),
                &[0; 8],
            ]),
        );
        let mvex = mk_box(b"mvex", &trex);
        let moov = mk_box(b"moov", &cat(&[&mvhd, &trak, &mvex]));
        cat(&[&mk_box(b"ftyp", b"iso5\0\0\0\0"), &moov])
    }

    /// One fragment of `samples` 4-byte samples. With `durations`, the trun
    /// carries them; otherwise the trex default applies. With
    /// `absolute_base`, the tfhd carries an absolute `base_data_offset` (the
    /// mdat payload's position, given the fragment starts at `at`).
    fn mk_fragment(
        at: usize,
        samples: u32,
        durations: Option<u32>,
        absolute_base: bool,
    ) -> Vec<u8> {
        let build = |base: u64| {
            let tfhd = if absolute_base {
                mk_full_box(
                    b"tfhd",
                    0,
                    0x01,
                    &cat(&[&1u32.to_be_bytes(), &base.to_be_bytes()]),
                )
            } else {
                mk_full_box(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes())
            };
            let flags = if durations.is_some() { 0x300 } else { 0x200 };
            let mut body = samples.to_be_bytes().to_vec();
            for _ in 0..samples {
                if let Some(d) = durations {
                    body.extend_from_slice(&d.to_be_bytes());
                }
                body.extend_from_slice(&4u32.to_be_bytes());
            }
            let trun = mk_full_box(b"trun", 0, flags, &body);
            let traf = mk_box(b"traf", &cat(&[&tfhd, &trun]));
            mk_box(b"moof", &traf)
        };
        let moof_len = build(0).len();
        let payload: Vec<u8> = (0..samples * 4).map(|i| (i % 251) as u8).collect();
        let base = (at + moof_len + 8) as u64;
        cat(&[&build(base), &mk_box(b"mdat", &payload)])
    }

    fn mk_file(absolute_base: bool) -> Vec<u8> {
        let mut file = mk_init(None);
        file.extend(mk_fragment(file.len(), 10, None, absolute_base));
        file.extend(mk_fragment(file.len(), 5, Some(2_048), absolute_base));
        file
    }

    /// Ticks the test file holds: 10 default frames + 5 double frames.
    const MEDIA_TICKS: u64 = 10 * 1_024 + 5 * 2_048;

    fn path_box(buf: &[u8], path: &[[u8; 4]]) -> BoxRef {
        find_box_path(buf, 0, buf.len(), path).unwrap().unwrap()
    }

    fn duration_of(buf: &[u8], bref: BoxRef) -> u64 {
        let (_, field) = timescale_and_duration(buf, bref).unwrap();
        read_uint(buf, field.at, field.wide).unwrap()
    }

    #[test]
    fn mdhd_should_hold_the_summed_sample_durations() {
        let (out, _) = fixed(mk_file(false));
        let mdhd = path_box(&out, &[*b"moov", *b"trak", *b"mdia", *b"mdhd"]);
        assert_eq!(duration_of(&out, mdhd), MEDIA_TICKS);
    }

    #[test]
    fn mvhd_should_hold_the_duration_in_movie_timescale() {
        let (out, _) = fixed(mk_file(false));
        let mvhd = path_box(&out, &[*b"moov", *b"mvhd"]);
        assert_eq!(
            duration_of(&out, mvhd),
            rescale(MEDIA_TICKS, MEDIA_TS, MOVIE_TS)
        );
    }

    #[test]
    fn tkhd_should_hold_the_duration_in_movie_timescale() {
        let (out, _) = fixed(mk_file(false));
        let tkhd = path_box(&out, &[*b"moov", *b"trak", *b"tkhd"]);
        let (_, field) = tkhd_fields(&out, tkhd).unwrap();
        assert_eq!(
            read_uint(&out, field.at, field.wide).unwrap(),
            rescale(MEDIA_TICKS, MEDIA_TS, MOVIE_TS)
        );
    }

    #[test]
    fn milliseconds_should_be_returned() {
        let (_, millis) = fixed(mk_file(false));
        // 20 480 ticks at 48 kHz = 426.67 ms.
        assert_eq!(millis, 427);
    }

    #[test]
    fn mehd_should_be_added_as_the_first_mvex_child() {
        let (out, _) = fixed(mk_file(false));
        let mvex = path_box(&out, &[*b"moov", *b"mvex"]);
        let (t, mehd) = read_box(&out, mvex.payload).unwrap().unwrap();
        let (_, _, body) = full_box_header(&out, mehd).unwrap();
        assert_eq!(
            (t, read_uint(&out, body, false).unwrap()),
            (*b"mehd", rescale(MEDIA_TICKS, MEDIA_TS, MOVIE_TS))
        );
    }

    #[test]
    fn moov_and_mvex_should_grow_by_the_mehd_size() {
        let input = mk_file(false);
        let moov_before = path_box(&input, &[*b"moov"]).size;
        let mvex_before = path_box(&input, &[*b"moov", *b"mvex"]).size;
        let (out, _) = fixed(input);
        assert_eq!(
            (
                path_box(&out, &[*b"moov"]).size,
                path_box(&out, &[*b"moov", *b"mvex"]).size
            ),
            (
                moov_before + MEHD_V0_SIZE as usize,
                mvex_before + MEHD_V0_SIZE as usize
            )
        );
    }

    #[test]
    fn every_top_level_box_should_still_parse_after_the_insertion() {
        let (out, _) = fixed(mk_file(false));
        let mut types = Vec::new();
        let mut pos = 0;
        while let Some((t, b)) = read_box(&out, pos).unwrap() {
            types.push(t);
            pos = b.end();
        }
        assert_eq!(
            types,
            [*b"ftyp", *b"moov", *b"moof", *b"mdat", *b"moof", *b"mdat"]
        );
    }

    /// The payload each fragment's `tfhd.base_data_offset` points at.
    fn based_payloads(buf: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((t, moof)) = read_box(buf, pos).unwrap() {
            pos = moof.end();
            if t != *b"moof" {
                continue;
            }
            let tfhd = find_box_path(buf, moof.payload, moof.end(), &[*b"traf", *b"tfhd"])
                .unwrap()
                .unwrap();
            let base = parse_tfhd(buf, tfhd).unwrap().base_data_offset.unwrap();
            let base = usize::try_from(base).unwrap();
            out.push(buf[base..base + 8].to_vec());
        }
        out
    }

    #[test]
    fn absolute_base_data_offsets_should_follow_the_moved_data() {
        let input = mk_file(true);
        let before = based_payloads(&input);
        let (out, _) = fixed(input);
        assert_eq!(based_payloads(&out), before);
    }

    #[test]
    fn chunk_offsets_past_the_insertion_should_be_shifted() {
        let mut input = mk_init(Some(0));
        let chunk_at = u32::try_from(input.len()).unwrap();
        input = mk_init(Some(chunk_at));
        input.extend(mk_fragment(input.len(), 3, None, false));
        let (out, _) = fixed(input);
        let stco = path_box(
            &out,
            &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stco"],
        );
        let (_, _, body) = full_box_header(&out, stco).unwrap();
        assert_eq!(
            read_uint(&out, body + 4, false).unwrap(),
            u64::from(chunk_at) + u64::from(MEHD_V0_SIZE)
        );
    }

    #[test]
    fn running_twice_should_not_add_a_second_mehd() {
        let (once, _) = fixed(mk_file(false));
        let (twice, _) = fixed(once.clone());
        assert_eq!(twice, once);
    }

    #[test]
    fn samples_without_any_duration_should_be_an_error() {
        let mut file = mk_init(None);
        // Zero the trex default duration (the last 12 bytes of the init are
        // trex's duration, size and flags).
        let len = file.len();
        file[len - 12..len - 8].copy_from_slice(&0u32.to_be_bytes());
        file.extend(mk_fragment(file.len(), 2, None, false));
        assert!(matches!(
            write_duration(&mut file),
            Err(DrmError::Decrypt(_))
        ));
    }

    #[test]
    fn file_without_moov_should_be_an_error() {
        let mut file = mk_box(b"ftyp", b"iso5");

        assert!(matches!(
            write_duration(&mut file),
            Err(DrmError::Decrypt(_))
        ));
    }

    #[test]
    fn mehd_should_widen_to_version_1_past_u32() {
        let wide = mehd_box(u64::from(u32::MAX) + 1);
        assert_eq!((wide.len(), wide[8]), (MEHD_V1_SIZE as usize, 1));
    }

    #[test]
    fn rescale_should_round_to_nearest() {
        assert_eq!(rescale(1_023, 48_000, 1_000), 21);
        assert_eq!(rescale(1_049, 48_000, 1_000), 22);
    }
}
