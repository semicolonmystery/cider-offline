//! Unencrypted HLS playlists for local files, and recognising Apple's
//! enhanced-HLS playlist URLs.
//!
//! When Cider plays a song's spatial version it does not use the
//! `WebPlayback` asset. It takes the song's `extendedAssetUrls.enhancedHls`
//! master playlist from the catalog, picks the binaural variant playlist from
//! it and streams that variant's encrypted fMP4 segments (byte ranges of one
//! file), unlocking them with the key-server URLs of the `WebPlayback` answer.
//!
//! For a song whose spatial copy is downloaded, [`crate::routing`] points
//! `enhancedHls` at [`local_master_url`] instead. That master lists one
//! variant, [`local_media_url`], whose segments are byte ranges of the local
//! file served by the media endpoint (which supports `Range`). The playlists
//! carry no `#EXT-X-KEY`, so no license is needed. The layout (init segment,
//! fragments and their durations) is read from the file itself with
//! [`read_layout`]; downloads are fragmented MP4 like Apple's streams.

// `write!` into a `String` cannot fail.
use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::media::{MEDIA_HOST, variant_media_url};
use crate::storage::AudioVariant;

/// Path prefix of the local playlists: `<prefix><media id>/<variant>/<file>`.
pub(crate) const HLS_URL_PREFIX: &str = "/__cider-offline/hls/";
/// File name of a local master playlist.
const MASTER_FILE: &str = "master.m3u8";
/// File name of a local media (variant) playlist.
const MEDIA_FILE: &str = "media.m3u8";
/// `Content-Type` of the playlists, as Apple serves them.
pub(crate) const PLAYLIST_CONTENT_TYPE: &str = "application/x-mpegURL";
/// Fragments shorter than this are joined into one segment (Apple's
/// fragments, 10 s normal and 15 s spatial, each stay a segment).
const TARGET_SEGMENT_SECS: f64 = 5.0;
/// Largest `moov` / `moof` box read into memory.
const MAX_BOX_READ: u64 = 32 * 1024 * 1024;
/// The audio group Cider picks from enhanced-HLS masters when its spatial
/// setting is on.
const BINAURAL_GROUP: &str = "audio-stereo-256-binaural";
/// The plain stereo audio group of enhanced-HLS masters.
const STEREO_GROUP: &str = "audio-stereo-256";

/// Which playlist of a song a URL asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaylistKind {
    /// The master playlist listing the variants.
    Master,
    /// A variant's media playlist listing the segments.
    Media,
}

/// The URL of the local master playlist serving `variant` of `media_id`.
pub(crate) fn local_master_url(media_id: &str, variant: AudioVariant) -> String {
    format!("https://{MEDIA_HOST}{HLS_URL_PREFIX}{media_id}/{variant}/{MASTER_FILE}")
}

/// The URL of the local media playlist serving `variant` of `media_id`.
pub(crate) fn local_media_url(media_id: &str, variant: AudioVariant) -> String {
    format!("https://{MEDIA_HOST}{HLS_URL_PREFIX}{media_id}/{variant}/{MEDIA_FILE}")
}

/// Parses a local playlist path (`<prefix><id>/<variant>/<file>`).
pub(crate) fn parse_local_path(path: &str) -> Option<(&str, AudioVariant, PlaylistKind)> {
    let mut segments = path.strip_prefix(HLS_URL_PREFIX)?.split('/');
    let id = segments
        .next()
        .filter(|id| crate::media::is_valid_media_id(id) && !id.bytes().all(|b| b == b'.'))?;
    let variant = segments.next().and_then(AudioVariant::parse)?;
    let kind = match segments.next()? {
        MASTER_FILE => PlaylistKind::Master,
        MEDIA_FILE => PlaylistKind::Media,
        _ => return None,
    };
    segments.next().is_none().then_some((id, variant, kind))
}

/// An Apple enhanced-HLS playlist request, recognised by its file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplePlaylist {
    /// A master (`P<n>_default.m3u8` and the like); it names no song, so the
    /// song comes from the catalog `enhancedHls` URLs seen earlier.
    Master,
    /// A variant (`P<n>_A<adam id>_audio_….m3u8`), naming its song.
    Variant {
        /// The catalog (adam) id in the file name.
        adam_id: String,
    },
}

/// Classifies an Apple audio-host path as an enhanced-HLS playlist. The
/// classic `WebPlayback` assets (`mzaf_….m3u8`) and segments are `None`.
pub(crate) fn apple_playlist(path: &str) -> Option<ApplePlaylist> {
    let name = path.rsplit('/').next()?;
    let stem = name.strip_suffix(".m3u8")?;
    let rest = stem.strip_prefix('P')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &rest[digits..];
    if let Some(after) = rest.strip_prefix("_A") {
        let adam_id: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !adam_id.is_empty() {
            return Some(ApplePlaylist::Variant { adam_id });
        }
    }
    rest.starts_with('_').then_some(ApplePlaylist::Master)
}

/// The key a master playlist URL is remembered by: lower-cased host (no
/// port) and path, without the query.
pub(crate) fn url_key(host: &str, path: &str) -> String {
    let host = host.split(':').next().unwrap_or_default();
    let host = host.strip_suffix('.').unwrap_or(host);
    format!("{}{path}", host.to_ascii_lowercase())
}

/// [`url_key`] of an absolute URL string.
pub(crate) fn url_key_of(url: &str) -> Option<String> {
    let uri: hudsucker::hyper::Uri = url.parse().ok()?;
    Some(url_key(uri.host()?, uri.path()))
}

/// Why a file's HLS layout could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LayoutError {
    /// Reading the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file is not a fragmented MP4 with one AAC track.
    #[error("not a fragmented AAC MP4: {0}")]
    Format(&'static str),
}

/// One HLS segment: a byte range of the file and how long it plays.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    /// First byte.
    pub(crate) offset: u64,
    /// Length in bytes.
    pub(crate) length: u64,
    /// Play time in seconds.
    pub(crate) duration_secs: f64,
}

/// The HLS layout of a fragmented MP4 file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layout {
    /// Length of the init segment (`ftyp` + `moov`) at offset 0.
    pub(crate) init_length: u64,
    /// The media segments, in file order.
    pub(crate) segments: Vec<Segment>,
    /// Size of the whole file.
    pub(crate) file_size: u64,
}

impl Layout {
    /// Total play time in seconds.
    pub(crate) fn duration_secs(&self) -> f64 {
        self.segments.iter().map(|s| s.duration_secs).sum()
    }

    /// Average bit rate (bits per second), for `BANDWIDTH`.
    fn bandwidth(&self) -> u64 {
        let secs = self.duration_secs();
        if secs <= 0.0 {
            return 0;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a positive bit rate far below 2^52"
        )]
        let bandwidth = (self.file_size as f64 * 8.0 / secs).round() as u64;
        bandwidth
    }
}

/// Reads the HLS layout of the file at `path` (blocking I/O).
pub(crate) fn read_layout(path: &Path) -> Result<Layout, LayoutError> {
    let mut file = std::fs::File::open(path)?;
    layout_of(&mut file)
}

/// The track facts needed to time fragments.
#[derive(Debug, Clone, Copy)]
struct TrackInfo {
    track_id: u32,
    timescale: u32,
    default_duration: u32,
}

/// A run of top-level boxes forming one fragment (`[styp] moof mdat …`).
#[derive(Debug, Clone, Copy)]
struct Chunk {
    start: u64,
    end: u64,
    ticks: u64,
    has_moof: bool,
}

/// Reads the HLS layout of a fragmented MP4 from `reader`.
pub(crate) fn layout_of<R: Read + Seek>(reader: &mut R) -> Result<Layout, LayoutError> {
    let file_size = reader.seek(SeekFrom::End(0))?;
    let mut info: Option<TrackInfo> = None;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut open: Option<Chunk> = None;
    let mut pos = 0u64;
    while pos + 8 <= file_size {
        reader.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        reader.read_exact(&mut header)?;
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let kind = [header[4], header[5], header[6], header[7]];
        let (size, header_len) = match size32 {
            0 => (file_size - pos, 8),
            1 => {
                let mut large = [0u8; 8];
                reader.read_exact(&mut large)?;
                (u64::from_be_bytes(large), 16)
            }
            n => (u64::from(n), 8),
        };
        if size < header_len || pos + size > file_size {
            return Err(LayoutError::Format("truncated box"));
        }
        let end = pos + size;
        match &kind {
            b"moov" => {
                let payload = read_payload(reader, size - header_len)?;
                info = Some(parse_moov(&payload)?);
            }
            b"moof" => {
                let track = info.ok_or(LayoutError::Format("moof before moov"))?;
                let payload = read_payload(reader, size - header_len)?;
                let ticks = moof_ticks(&payload, track)?;
                if open.is_some_and(|c| c.has_moof) {
                    chunks.extend(open.take());
                }
                let chunk = open.get_or_insert(Chunk {
                    start: pos,
                    end,
                    ticks: 0,
                    has_moof: false,
                });
                chunk.has_moof = true;
                chunk.ticks += ticks;
                chunk.end = end;
            }
            b"styp" | b"sidx" | b"prft" | b"emsg" => {
                if open.is_some_and(|c| c.has_moof) {
                    chunks.extend(open.take());
                }
                open.get_or_insert(Chunk {
                    start: pos,
                    end,
                    ticks: 0,
                    has_moof: false,
                })
                .end = end;
            }
            b"mfra" => break,
            _ => {
                if let Some(chunk) = open.as_mut() {
                    chunk.end = end;
                }
            }
        }
        pos = end;
    }
    chunks.extend(open.filter(|c| c.has_moof));
    let track = info.ok_or(LayoutError::Format("no moov"))?;
    let first = chunks.first().ok_or(LayoutError::Format("no fragments"))?;
    let init_length = first.start;
    let segments = group_segments(&chunks, track.timescale);
    if segments.iter().all(|s| s.duration_secs <= 0.0) {
        return Err(LayoutError::Format("fragments carry no duration"));
    }
    Ok(Layout {
        init_length,
        segments,
        file_size,
    })
}

/// Joins consecutive fragments into segments of at least
/// [`TARGET_SEGMENT_SECS`] (the last one may be shorter).
fn group_segments(chunks: &[Chunk], timescale: u32) -> Vec<Segment> {
    let secs = |ticks: u64| {
        #[expect(
            clippy::cast_precision_loss,
            reason = "tick counts of a song are far below 2^52"
        )]
        let secs = ticks as f64 / f64::from(timescale);
        secs
    };
    let mut segments = Vec::new();
    let mut current: Option<(u64, u64, u64)> = None;
    for chunk in chunks {
        let (start, _, ticks) = current.unwrap_or((chunk.start, chunk.end, 0));
        let joined = (start, chunk.end, ticks + chunk.ticks);
        if secs(joined.2) >= TARGET_SEGMENT_SECS {
            segments.push(segment(joined, secs(joined.2)));
            current = None;
        } else {
            current = Some(joined);
        }
    }
    if let Some(rest) = current {
        segments.push(segment(rest, secs(rest.2)));
    }
    segments
}

fn segment((start, end, _): (u64, u64, u64), duration_secs: f64) -> Segment {
    Segment {
        offset: start,
        length: end - start,
        duration_secs,
    }
}

fn read_payload<R: Read>(reader: &mut R, len: u64) -> Result<Vec<u8>, LayoutError> {
    if len > MAX_BOX_READ {
        return Err(LayoutError::Format("box too large"));
    }
    let mut payload =
        vec![0u8; usize::try_from(len).map_err(|_| LayoutError::Format("box too large"))?];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

/// Iterates the child boxes of a box payload as `(type, payload)`; stops at
/// the first malformed header.
fn children(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let kind = [rest[4], rest[5], rest[6], rest[7]];
        let (size, header) = match size {
            0 => (rest.len(), 8),
            1 => {
                let large = u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?);
                (usize::try_from(large).ok()?, 16)
            }
            n => (usize::try_from(n).ok()?, 8),
        };
        if size < header || size > rest.len() {
            return None;
        }
        let payload = &rest[header..size];
        rest = &rest[size..];
        Some((kind, payload))
    })
}

fn child(data: &[u8], kind: [u8; 4]) -> Option<&[u8]> {
    children(data).find(|(k, _)| *k == kind).map(|(_, p)| p)
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Reads the first track's id, timescale, codec and fragment defaults.
fn parse_moov(moov: &[u8]) -> Result<TrackInfo, LayoutError> {
    let trak = child(moov, *b"trak").ok_or(LayoutError::Format("no trak"))?;
    let tkhd = child(trak, *b"tkhd").ok_or(LayoutError::Format("no tkhd"))?;
    let track_id = match tkhd.first() {
        Some(1) => u32_at(tkhd, 20),
        _ => u32_at(tkhd, 12),
    }
    .ok_or(LayoutError::Format("short tkhd"))?;
    let mdia = child(trak, *b"mdia").ok_or(LayoutError::Format("no mdia"))?;
    let mdhd = child(mdia, *b"mdhd").ok_or(LayoutError::Format("no mdhd"))?;
    let timescale = match mdhd.first() {
        Some(1) => u32_at(mdhd, 20),
        _ => u32_at(mdhd, 12),
    }
    .filter(|&t| t > 0)
    .ok_or(LayoutError::Format("bad mdhd"))?;
    let stsd = child(mdia, *b"minf")
        .and_then(|minf| child(minf, *b"stbl"))
        .and_then(|stbl| child(stbl, *b"stsd"))
        .ok_or(LayoutError::Format("no stsd"))?;
    let codec = stsd
        .get(8..)
        .and_then(|entries| children(entries).next())
        .map(|(kind, _)| kind);
    if codec != Some(*b"mp4a") {
        return Err(LayoutError::Format("not AAC"));
    }
    let default_duration = child(moov, *b"mvex")
        .map(|mvex| {
            children(mvex)
                .filter(|(kind, _)| kind == b"trex")
                .find(|(_, trex)| u32_at(trex, 4) == Some(track_id))
                .and_then(|(_, trex)| u32_at(trex, 12))
                .unwrap_or(0)
        })
        .ok_or(LayoutError::Format("not fragmented (no mvex)"))?;
    Ok(TrackInfo {
        track_id,
        timescale,
        default_duration,
    })
}

/// The play time of one `moof`, in the track's timescale.
fn moof_ticks(moof: &[u8], track: TrackInfo) -> Result<u64, LayoutError> {
    let mut ticks = 0u64;
    for (_, traf) in children(moof).filter(|(kind, _)| kind == b"traf") {
        let tfhd = child(traf, *b"tfhd").ok_or(LayoutError::Format("no tfhd"))?;
        let flags = u32_at(tfhd, 0).ok_or(LayoutError::Format("short tfhd"))? & 0x00ff_ffff;
        if u32_at(tfhd, 4) != Some(track.track_id) {
            continue;
        }
        let mut offset = 8;
        if flags & 0x01 != 0 {
            offset += 8;
        }
        if flags & 0x02 != 0 {
            offset += 4;
        }
        let default_duration = if flags & 0x08 != 0 {
            u32_at(tfhd, offset).ok_or(LayoutError::Format("short tfhd"))?
        } else {
            track.default_duration
        };
        for (_, trun) in children(traf).filter(|(kind, _)| kind == b"trun") {
            ticks += trun_ticks(trun, default_duration)?;
        }
    }
    Ok(ticks)
}

/// The summed sample durations of one `trun`.
fn trun_ticks(trun: &[u8], default_duration: u32) -> Result<u64, LayoutError> {
    let flags = u32_at(trun, 0).ok_or(LayoutError::Format("short trun"))? & 0x00ff_ffff;
    let count = u32_at(trun, 4).ok_or(LayoutError::Format("short trun"))?;
    let mut offset = 8;
    if flags & 0x01 != 0 {
        offset += 4;
    }
    if flags & 0x04 != 0 {
        offset += 4;
    }
    if flags & 0x100 == 0 {
        return Ok(u64::from(count) * u64::from(default_duration));
    }
    let per_sample = [0x100, 0x200, 0x400, 0x800]
        .iter()
        .filter(|&&bit| flags & bit != 0)
        .count()
        * 4;
    let mut ticks = 0u64;
    for index in 0..usize::try_from(count).map_err(|_| LayoutError::Format("short trun"))? {
        let duration =
            u32_at(trun, offset + index * per_sample).ok_or(LayoutError::Format("short trun"))?;
        ticks += u64::from(duration);
    }
    Ok(ticks)
}

/// A master playlist whose only variant is `media_url`.
///
/// It is listed under both the binaural and the stereo audio group of
/// Apple's enhanced-HLS masters, so whichever one Cider prefers resolves to
/// the local file.
pub(crate) fn master_playlist(layout: &Layout, media_url: &str) -> String {
    let bandwidth = layout.bandwidth();
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    for (group, channels) in [(BINAURAL_GROUP, "2/-/BINAURAL"), (STEREO_GROUP, "2")] {
        let _ = writeln!(
            out,
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{group}\",AUTOSELECT=YES,CHANNELS=\"{channels}\",NAME=\"songEnhanced\""
        );
    }
    out.push('\n');
    for group in [BINAURAL_GROUP, STEREO_GROUP] {
        let _ = writeln!(
            out,
            "#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH={bandwidth},BANDWIDTH={bandwidth},CODECS=\"mp4a.40.2\",AUDIO=\"{group}\"\n{media_url}"
        );
    }
    out
}

/// A VOD media playlist of `layout`, every segment a byte range of
/// `file_url`, with no encryption.
pub(crate) fn media_playlist(layout: &Layout, file_url: &str) -> String {
    let target = layout
        .segments
        .iter()
        .map(|s| s.duration_secs)
        .fold(1.0_f64, f64::max)
        .ceil();
    let mut out = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-VERSION:7\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-MAP:URI=\"{file_url}\",BYTERANGE=\"{}@0\"\n",
        layout.init_length
    );
    for segment in &layout.segments {
        let _ = writeln!(
            out,
            "#EXTINF:{:.5},\n#EXT-X-BYTERANGE:{}@{}\n{file_url}",
            segment.duration_secs, segment.length, segment.offset
        );
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// The playlist of `kind` for `variant` of `media_id`, built from the file
/// at `path` (blocking I/O).
pub(crate) fn playlist_for(
    path: &Path,
    media_id: &str,
    variant: AudioVariant,
    kind: PlaylistKind,
) -> Result<String, LayoutError> {
    let layout = read_layout(path)?;
    Ok(match kind {
        PlaylistKind::Master => master_playlist(&layout, &local_media_url(media_id, variant)),
        PlaylistKind::Media => media_playlist(&layout, &variant_media_url(media_id, variant)),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use super::*;

    fn mp4_box(kind: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(payload.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn full_box(kind: &[u8], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut payload = (u32::from(version) << 24 | flags).to_be_bytes().to_vec();
        payload.extend_from_slice(body);
        mp4_box(kind, &payload)
    }

    fn be(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// `ftyp` + `moov` of one AAC track (id 1, timescale 48000, trex default
    /// duration 1024).
    pub(crate) fn init_segment(codec: &[u8]) -> Vec<u8> {
        let tkhd = full_box(b"tkhd", 0, 7, &be(&[0, 0, 1, 0, 0]));
        let mdhd = full_box(b"mdhd", 0, 0, &be(&[0, 0, 48_000, 0, 0]));
        let entry = mp4_box(codec, &[0u8; 28]);
        let mut stsd_body = be(&[1]);
        stsd_body.extend(entry);
        let stsd = full_box(b"stsd", 0, 0, &stsd_body);
        let stbl = mp4_box(b"stbl", &stsd);
        let minf = mp4_box(b"minf", &stbl);
        let mut mdia_body = mdhd;
        mdia_body.extend(minf);
        let mdia = mp4_box(b"mdia", &mdia_body);
        let mut trak_body = tkhd;
        trak_body.extend(mdia);
        let trak = mp4_box(b"trak", &trak_body);
        let trex = full_box(b"trex", 0, 0, &be(&[1, 1, 1024, 0, 0]));
        let mvex = mp4_box(b"mvex", &trex);
        let mut moov_body = full_box(b"mvhd", 0, 0, &[0u8; 96]);
        moov_body.extend(trak);
        moov_body.extend(mvex);
        let mut out = mp4_box(b"ftyp", b"iso6\0\0\0\0");
        out.extend(mp4_box(b"moov", &moov_body));
        out
    }

    /// A `moof` + `mdat` of `samples` samples; per-sample durations of 1024
    /// when `explicit`, else the trex default.
    pub(crate) fn fragment(samples: u32, explicit: bool) -> Vec<u8> {
        let tfhd = full_box(b"tfhd", 0, 0x02_0000, &be(&[1]));
        let trun = if explicit {
            let mut body = be(&[samples, 0]);
            for _ in 0..samples {
                body.extend(be(&[1024, 10]));
            }
            full_box(b"trun", 0, 0x01 | 0x100 | 0x200, &body)
        } else {
            full_box(b"trun", 0, 0x01, &be(&[samples, 0]))
        };
        let mut traf_body = tfhd;
        traf_body.extend(trun);
        let traf = mp4_box(b"traf", &traf_body);
        let mut moof_body = full_box(b"mfhd", 0, 0, &be(&[1]));
        moof_body.extend(traf);
        let mut out = mp4_box(b"moof", &moof_body);
        out.extend(mp4_box(b"mdat", &[0u8; 100]));
        out
    }

    /// A file of `fragments` fragments, each 4.9067 s (230 samples of 1024
    /// at 48 kHz).
    pub(crate) fn sample_file(fragments: usize) -> Vec<u8> {
        let mut file = init_segment(b"mp4a");
        for index in 0..fragments {
            file.extend(fragment(230, index % 2 == 0));
        }
        file
    }

    #[test]
    fn layout_should_start_segments_after_the_init_segment() {
        let file = sample_file(3);
        let layout = layout_of(&mut Cursor::new(&file)).unwrap();
        assert_eq!(
            layout.init_length,
            u64::try_from(init_segment(b"mp4a").len()).unwrap()
        );
    }

    #[test]
    fn layout_should_join_short_fragments_into_segments() {
        let layout = layout_of(&mut Cursor::new(sample_file(5))).unwrap();
        let durations: Vec<String> = layout
            .segments
            .iter()
            .map(|s| format!("{:.4}", s.duration_secs))
            .collect();
        assert_eq!(durations, ["9.8133", "9.8133", "4.9067"]);
    }

    #[test]
    fn segments_should_cover_the_file_contiguously() {
        let file = sample_file(5);
        let layout = layout_of(&mut Cursor::new(&file)).unwrap();
        let mut next = layout.init_length;
        for segment in &layout.segments {
            assert_eq!(segment.offset, next);
            next += segment.length;
        }
        assert_eq!(next, u64::try_from(file.len()).unwrap());
    }

    #[test]
    fn non_aac_file_should_be_refused() {
        let mut file = init_segment(b"alac");
        file.extend(fragment(10, true));
        assert!(layout_of(&mut Cursor::new(file)).is_err());
    }

    #[test]
    fn unfragmented_file_should_be_refused() {
        let file = init_segment(b"mp4a");
        assert!(layout_of(&mut Cursor::new(file)).is_err());
    }

    #[test]
    fn media_playlist_should_list_byte_ranges_without_keys() {
        let layout = layout_of(&mut Cursor::new(sample_file(2))).unwrap();
        let playlist = media_playlist(&layout, "https://h/f");
        let expected = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-VERSION:7\n#EXT-X-MEDIA-SEQUENCE:0\n\
#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-MAP:URI=\"https://h/f\",BYTERANGE=\"{init}@0\"\n\
#EXTINF:9.81333,\n#EXT-X-BYTERANGE:{len}@{init}\nhttps://h/f\n#EXT-X-ENDLIST\n",
            init = layout.init_length,
            len = layout.segments[0].length,
        );
        assert_eq!(playlist, expected);
    }

    #[test]
    fn master_playlist_should_offer_the_local_variant_in_both_groups() {
        let layout = layout_of(&mut Cursor::new(sample_file(2))).unwrap();
        let master = master_playlist(&layout, "https://h/media.m3u8");
        let groups: Vec<&str> = master
            .lines()
            .filter(|l| l.starts_with("#EXT-X-STREAM-INF"))
            .filter_map(|l| l.split("AUDIO=\"").nth(1))
            .collect();
        let uris = master
            .lines()
            .filter(|l| *l == "https://h/media.m3u8")
            .count();
        assert_eq!(
            (groups, uris),
            (vec!["audio-stereo-256-binaural\"", "audio-stereo-256\""], 2)
        );
    }

    #[test]
    fn local_urls_should_parse_back() {
        let master = local_master_url("a.1657271974", AudioVariant::Spatial);
        let media = local_media_url("a.1657271974", AudioVariant::Spatial);
        let path = |url: &str| {
            url.parse::<hudsucker::hyper::Uri>()
                .unwrap()
                .path()
                .to_owned()
        };
        assert_eq!(
            (
                parse_local_path(&path(&master)),
                parse_local_path(&path(&media))
            ),
            (
                Some(("a.1657271974", AudioVariant::Spatial, PlaylistKind::Master)),
                Some(("a.1657271974", AudioVariant::Spatial, PlaylistKind::Media))
            )
        );
    }

    #[test]
    fn local_path_should_reject_unknown_files_and_traversal() {
        assert_eq!(
            (
                parse_local_path("/__cider-offline/hls/1/spatial/x.m3u8"),
                parse_local_path("/__cider-offline/hls/../spatial/master.m3u8"),
                parse_local_path("/__cider-offline/hls/1/loud/master.m3u8"),
            ),
            (None, None, None)
        );
    }

    #[test]
    fn apple_variant_playlist_should_name_its_song() {
        // Recorded request path (Creepin', 2026-11-20 log).
        let path = "/itunes-assets/HLSMusic211/v4/87/c8/06/87c80620-cd16-7d25-9ff0-abfa68e250cf/P1249902232_A1657271974_audio_en_gr256_mp4a-40-2_bm.m3u8";
        assert_eq!(
            apple_playlist(path),
            Some(ApplePlaylist::Variant {
                adam_id: "1657271974".to_owned()
            })
        );
    }

    #[test]
    fn apple_master_playlist_should_be_recognised() {
        let path = "/itunes-assets/HLSMusic211/v4/87/c8/06/87c80620-cd16-7d25-9ff0-abfa68e250cf/P1249902232_default.m3u8";
        assert_eq!(apple_playlist(path), Some(ApplePlaylist::Master));
    }

    #[test]
    fn webplayback_assets_and_segments_should_not_be_enhanced_playlists() {
        let asset =
            "/itunes-assets/Music221/v4/cc/dd/fc/ccddfc36/mzaf_A1818817443.rphq.aac.wa.m3u8";
        let segment = "/itunes-assets/x/P1249902232_A1657271974_audio_en_gr256_mp4a-40-2_bm_m.mp4";
        assert_eq!(
            (apple_playlist(asset), apple_playlist(segment)),
            (None, None)
        );
    }

    /// Manual check on a real download: `CIDER_MEDIA_FILE=<copy of a .m4a>`
    /// prints its layout and writes `<file>.m3u8` next to it, pointing at
    /// the file by name, for `ffmpeg -i <file>.m3u8 -f null -`.
    #[test]
    #[ignore = "needs CIDER_MEDIA_FILE"]
    fn real_file_layout() {
        let path = std::path::PathBuf::from(std::env::var("CIDER_MEDIA_FILE").unwrap());
        let layout = read_layout(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(path.with_extension("m3u8"), media_playlist(&layout, &name)).unwrap();
        let durations: Vec<String> = layout
            .segments
            .iter()
            .map(|s| format!("{:.3}", s.duration_secs))
            .collect();
        #[expect(clippy::print_stdout, reason = "manual check output")]
        {
            println!(
                "init {} bytes, {} segments, {:.3} s, {} bytes, {} bit/s: {durations:?}",
                layout.init_length,
                layout.segments.len(),
                layout.duration_secs(),
                layout.file_size,
                layout.bandwidth()
            );
        }
    }

    #[test]
    fn url_key_should_ignore_port_case_and_query() {
        assert_eq!(
            (
                url_key("AOD.itunes.apple.com:443", "/a/P1_default.m3u8"),
                url_key_of("https://aod.itunes.apple.com/a/P1_default.m3u8?x=1"),
            ),
            (
                "aod.itunes.apple.com/a/P1_default.m3u8".to_owned(),
                Some("aod.itunes.apple.com/a/P1_default.m3u8".to_owned())
            )
        );
    }
}
