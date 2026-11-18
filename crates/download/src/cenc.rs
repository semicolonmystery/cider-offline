//! CENC (Common Encryption) decryption for ISO BMFF / fragmented MP4.
//!
//! Handles AES-128-CTR (`cenc` scheme) with full-sample and subsample
//! encryption, which is what Apple Music uses for the `28:ctrp256` flavor,
//! and AES-128-CBC with pattern encryption (`cbcs` scheme).
//!
//! The box parser follows "parse, don't validate": every malformed length is
//! reported as a [`DrmError::Decrypt`] at the single point of reading
//! ([`read_box`]). Callers higher up — [`decrypt`] and its helpers — then
//! propagate errors through `?`, so a fragment that *looks* encrypted can
//! never be written back silently if any of the required metadata is missing
//! or malformed: writing it out would produce a file that plays as noise.

#![expect(
    clippy::struct_field_names,
    reason = "ISO/IEC 23001-7 spells every tenc field `default_*`; keeping the spec names helps when cross-reading the standard"
)]

use aes::cipher::{BlockCipherDecrypt, KeyInit};
use aes::{Aes128, Block};
use ctr::cipher::{KeyIvInit, StreamCipher};

use crate::error::DrmError;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Encryption scheme as declared by `moov/.../stsd/<sample_entry>/sinf/schm.scheme_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    /// AES-128-CTR (ISO/IEC 23001-7 clause 10.1). Used by Apple's `ctrp*` song flavors.
    Cenc,
    /// AES-128-CBC with sample-based pattern encryption (ISO/IEC 23001-7 clause 10.4).
    /// Used by Apple's music-video audio renditions. The IV is the `tenc` v1
    /// `default_constant_IV` reset at every sample, and the pattern is
    /// `default_crypt_byte_block : default_skip_byte_block` (0:0 means the whole
    /// protected range is encrypted).
    Cbcs,
}

/// Children of an `AudioSampleEntry` begin this many bytes after the entry's
/// own payload (6 reserved + 2 `data_reference_index` + 8 reserved +
/// 2 channelcount + 2 samplesize + 2 `pre_defined` + 2 reserved + 4 samplerate).
const AUDIO_SAMPLE_ENTRY_PRELUDE: usize = 28;

/// Children of a `VisualSampleEntry` begin this many bytes after the entry's
/// own payload (6 reserved + 2 `data_reference_index` + 16 `pre_defined` +
/// 2 width + 2 height + 4 horizresolution + 4 vertresolution + 4 reserved +
/// 2 `frame_count` + 32 compressorname + 2 depth + 2 `pre_defined`).
const VISUAL_SAMPLE_ENTRY_PRELUDE: usize = 78;

// ---------------------------------------------------------------------------
// Minimal MP4 box parser
// ---------------------------------------------------------------------------

/// A located MP4 box (offset + size in the file buffer).
#[derive(Debug, Clone, Copy)]
struct BoxRef {
    /// Offset of the box header in the buffer.
    offset: usize,
    /// Total box size (header + payload).
    size: usize,
    /// Offset of the payload (after header).
    payload: usize,
}

/// Reads a box header at `offset`.
///
/// `Ok(None)` means `offset == data.len()` exactly (clean end of buffer, no
/// more boxes). Any other out-of-range case, truncated header, or malformed
/// size is reported as [`DrmError::Decrypt`].
///
/// Supports the three ISO/IEC 14496-12 size encodings:
/// * `size == 0` — box extends to end of buffer;
/// * `size == 1` — a 64-bit `largesize` follows the type field;
/// * `size >= 8` — regular size field.
///
/// Sizes in `2..=7` and sizes that would run past the end of `data` are
/// rejected.
fn read_box(data: &[u8], offset: usize) -> Result<Option<([u8; 4], BoxRef)>, DrmError> {
    if offset == data.len() {
        return Ok(None);
    }
    if offset > data.len() {
        return Err(DrmError::Decrypt(format!(
            "malformed box: offset {offset} past buffer of {} bytes",
            data.len()
        )));
    }
    if offset + 8 > data.len() {
        return Err(DrmError::Decrypt(format!(
            "malformed box: truncated header at {offset} ({} bytes remain, need 8)",
            data.len() - offset
        )));
    }

    let raw_size = u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]) as usize;
    let box_type: [u8; 4] = [
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ];

    let (size, payload) = if raw_size == 1 {
        // 64-bit "largesize" follows the 4-byte size and 4-byte type.
        if offset + 16 > data.len() {
            return Err(DrmError::Decrypt(format!(
                "malformed box: truncated largesize header at {offset}"
            )));
        }
        let ext = u64::from_be_bytes([
            data[offset + 8],
            data[offset + 9],
            data[offset + 10],
            data[offset + 11],
            data[offset + 12],
            data[offset + 13],
            data[offset + 14],
            data[offset + 15],
        ]);
        if ext < 16 {
            return Err(DrmError::Decrypt(format!(
                "malformed box: largesize {ext} smaller than 16-byte header at {offset}"
            )));
        }
        let size = usize::try_from(ext).map_err(|_| {
            DrmError::Decrypt(format!("malformed box: largesize {ext} exceeds usize"))
        })?;
        (size, offset + 16)
    } else if raw_size == 0 {
        // Box extends to the end of the buffer.
        (data.len() - offset, offset + 8)
    } else if raw_size < 8 {
        return Err(DrmError::Decrypt(format!(
            "malformed box: size {raw_size} smaller than 8-byte header at {offset}"
        )));
    } else {
        (raw_size, offset + 8)
    };

    let end = offset
        .checked_add(size)
        .ok_or_else(|| DrmError::Decrypt(format!("malformed box: size overflow at {offset}")))?;
    if end > data.len() {
        return Err(DrmError::Decrypt(format!(
            "malformed box: end {end} past buffer of {} bytes (type {})",
            data.len(),
            String::from_utf8_lossy(&box_type),
        )));
    }

    Ok(Some((
        box_type,
        BoxRef {
            offset,
            size,
            payload,
        },
    )))
}

/// Payload of a full box (version + flags), returning `(version, flags, body_offset)`.
fn full_box_header(data: &[u8], bref: BoxRef) -> Option<(u8, u32, usize)> {
    let p = bref.payload;
    if p + 4 > data.len() {
        return None;
    }
    let version = data[p];
    let flags = u32::from(data[p + 1]) << 16 | u32::from(data[p + 2]) << 8 | u32::from(data[p + 3]);
    Some((version, flags, p + 4))
}

/// Finds the first box of `target` type in `data[start..end]`.
fn find_box(
    data: &[u8],
    start: usize,
    end: usize,
    target: [u8; 4],
) -> Result<Option<BoxRef>, DrmError> {
    let mut pos = start;
    while pos < end {
        let Some((t, b)) = read_box(data, pos)? else {
            break;
        };
        if b.offset + b.size > end {
            // A box declared larger than the region we're scanning is malformed
            // for this context, even if read_box accepted it against the full
            // buffer.
            return Err(DrmError::Decrypt(format!(
                "box {} at {} extends past region end {end}",
                String::from_utf8_lossy(&t),
                b.offset
            )));
        }
        if t == target {
            return Ok(Some(b));
        }
        pos = b.offset + b.size;
    }
    Ok(None)
}

/// Descends into nested boxes along `path`, returning the innermost box.
fn find_box_path(
    data: &[u8],
    start: usize,
    end: usize,
    path: &[[u8; 4]],
) -> Result<Option<BoxRef>, DrmError> {
    let mut s = start;
    let mut e = end;
    let mut result = None;
    for target in path {
        match find_box(data, s, e, *target)? {
            Some(bref) => {
                s = bref.payload;
                e = bref.offset + bref.size;
                result = Some(bref);
            }
            None => return Ok(None),
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Encryption metadata
// ---------------------------------------------------------------------------

/// Parsed `tenc` (`TrackEncryptionBox`), common to v0 and v1.
///
/// The v1 fields (`default_crypt_byte_block`, `default_skip_byte_block`,
/// `default_constant_iv`) are populated only when present on the wire and are
/// used by the `cbcs` scheme. For `cenc` only
/// `default_per_sample_iv_size` matters.
#[derive(Debug, Clone)]
struct TrackEncryption {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "parsed per ISO/IEC 23001-7; useful for cross-checking unencrypted tracks"
        )
    )]
    default_is_protected: u8,
    default_per_sample_iv_size: u8,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "parsed per ISO/IEC 23001-7; not needed for AES-CTR decryption itself"
        )
    )]
    default_kid: [u8; 16],
    default_crypt_byte_block: u8,
    default_skip_byte_block: u8,
    default_constant_iv: Option<Vec<u8>>,
}

fn parse_tenc(data: &[u8], bref: BoxRef) -> Result<TrackEncryption, DrmError> {
    let (version, _flags, body) = full_box_header(data, bref)
        .ok_or_else(|| DrmError::Decrypt("tenc: truncated full-box header".into()))?;

    // Common prefix (post FullBox header), ISO/IEC 23001-7 third edition:
    //   u8 reserved
    //   u8 reserved (v0) OR (default_crypt_byte_block << 4) | default_skip_byte_block (v1)
    //   u8 default_isProtected
    //   u8 default_Per_Sample_IV_Size
    //   u8[16] default_KID
    // Optionally, when is_protected == 1 && per_sample_iv_size == 0:
    //   u8 default_constant_IV_size
    //   u8[default_constant_IV_size] default_constant_IV
    if body + 20 > data.len() {
        return Err(DrmError::Decrypt("tenc: truncated body".into()));
    }
    let byte1 = data[body + 1];
    let (default_crypt_byte_block, default_skip_byte_block) = if version >= 1 {
        ((byte1 >> 4) & 0x0F, byte1 & 0x0F)
    } else {
        (0, 0)
    };
    let default_is_protected = data[body + 2];
    let default_per_sample_iv_size = data[body + 3];
    let mut default_kid = [0u8; 16];
    default_kid.copy_from_slice(&data[body + 4..body + 20]);

    let default_constant_iv = if default_is_protected == 1 && default_per_sample_iv_size == 0 {
        if body + 21 > data.len() {
            return Err(DrmError::Decrypt(
                "tenc: truncated default_constant_IV_size".into(),
            ));
        }
        let iv_size = data[body + 20] as usize;
        let iv_end = body
            .checked_add(21)
            .and_then(|s| s.checked_add(iv_size))
            .ok_or_else(|| DrmError::Decrypt("tenc: constant_IV offset overflow".into()))?;
        if iv_end > data.len() {
            return Err(DrmError::Decrypt(
                "tenc: truncated default_constant_IV".into(),
            ));
        }
        Some(data[body + 21..iv_end].to_vec())
    } else {
        None
    };

    Ok(TrackEncryption {
        default_is_protected,
        default_per_sample_iv_size,
        default_kid,
        default_crypt_byte_block,
        default_skip_byte_block,
        default_constant_iv,
    })
}

/// Parsed `schm` (SchemeTypeBox): the 4-char-code of the scheme, e.g. `cenc`.
fn parse_schm_scheme_type(data: &[u8], bref: BoxRef) -> Result<[u8; 4], DrmError> {
    let (_, _, body) = full_box_header(data, bref)
        .ok_or_else(|| DrmError::Decrypt("schm: truncated full-box header".into()))?;
    if body + 8 > data.len() {
        return Err(DrmError::Decrypt("schm: truncated body".into()));
    }
    Ok([data[body], data[body + 1], data[body + 2], data[body + 3]])
}

// ---------------------------------------------------------------------------
// Fragment headers (`tfhd`, `trun`, `senc`)
// ---------------------------------------------------------------------------

/// Fields of the `tfhd` box relevant to CENC decryption.
#[derive(Debug, Clone, Copy)]
struct TrackFragmentHeader {
    /// `track_ID`; keys the per-track timeline rebase
    /// ([`rebase_fragment_times`]).
    track_id: u32,
    /// Per-sample default when `trun` omits per-sample sizes (flag 0x200).
    default_sample_size: Option<u32>,
    /// Flag 0x020000: when set, sample-data base = this `moof`'s start.
    default_base_is_moof: bool,
    /// Flag 0x000001: explicit base-data-offset (overrides `default-base-is-moof`).
    base_data_offset: Option<u64>,
}

fn parse_tfhd(data: &[u8], bref: BoxRef) -> Result<TrackFragmentHeader, DrmError> {
    let (_, flags, body) = full_box_header(data, bref)
        .ok_or_else(|| DrmError::Decrypt("tfhd: truncated full-box header".into()))?;

    if body + 4 > data.len() {
        return Err(DrmError::Decrypt("tfhd: truncated track_ID".into()));
    }
    let track_id = u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]]);
    let mut off = body + 4;

    let base_data_offset = if flags & 0x0000_0001 != 0 {
        if off + 8 > data.len() {
            return Err(DrmError::Decrypt("tfhd: truncated base_data_offset".into()));
        }
        let v = u64::from_be_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
            data[off + 4],
            data[off + 5],
            data[off + 6],
            data[off + 7],
        ]);
        off += 8;
        Some(v)
    } else {
        None
    };

    // sample_description_index
    if flags & 0x0000_0002 != 0 {
        if off + 4 > data.len() {
            return Err(DrmError::Decrypt(
                "tfhd: truncated sample_description_index".into(),
            ));
        }
        off += 4;
    }

    // default_sample_duration
    if flags & 0x0000_0008 != 0 {
        if off + 4 > data.len() {
            return Err(DrmError::Decrypt(
                "tfhd: truncated default_sample_duration".into(),
            ));
        }
        off += 4;
    }

    let default_sample_size = if flags & 0x0000_0010 != 0 {
        if off + 4 > data.len() {
            return Err(DrmError::Decrypt(
                "tfhd: truncated default_sample_size".into(),
            ));
        }
        let v = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        off += 4;
        Some(v)
    } else {
        None
    };

    // default_sample_flags: present but unused here.
    if flags & 0x0000_0020 != 0 && off + 4 > data.len() {
        return Err(DrmError::Decrypt(
            "tfhd: truncated default_sample_flags".into(),
        ));
    }

    Ok(TrackFragmentHeader {
        track_id,
        default_sample_size,
        default_base_is_moof: flags & 0x0002_0000 != 0,
        base_data_offset,
    })
}

/// Fields of the `trun` box relevant to CENC decryption.
#[derive(Debug, Clone)]
struct TrackRunInfo {
    /// Flag 0x1: signed offset relative to the fragment's base-data location.
    data_offset: Option<i32>,
    /// Per-sample sizes, filled either from trun (flag 0x200) or from `tfhd`'s
    /// `default_sample_size`.
    sizes: Vec<u32>,
}

fn parse_trun(
    data: &[u8],
    bref: BoxRef,
    tfhd_default_sample_size: Option<u32>,
) -> Result<TrackRunInfo, DrmError> {
    let (_, flags, body) = full_box_header(data, bref)
        .ok_or_else(|| DrmError::Decrypt("trun: truncated full-box header".into()))?;
    if body + 4 > data.len() {
        return Err(DrmError::Decrypt("trun: truncated sample_count".into()));
    }
    let sample_count =
        u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]]) as usize;
    let mut off = body + 4;

    let data_offset = if flags & 0x0000_0001 != 0 {
        if off + 4 > data.len() {
            return Err(DrmError::Decrypt("trun: truncated data_offset".into()));
        }
        let v = i32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        off += 4;
        Some(v)
    } else {
        None
    };

    // first_sample_flags
    if flags & 0x0000_0004 != 0 {
        if off + 4 > data.len() {
            return Err(DrmError::Decrypt(
                "trun: truncated first_sample_flags".into(),
            ));
        }
        off += 4;
    }

    let has_duration = flags & 0x0000_0100 != 0;
    let has_size = flags & 0x0000_0200 != 0;
    let has_flags = flags & 0x0000_0400 != 0;
    let has_cts = flags & 0x0000_0800 != 0;

    let per_sample_bytes = (usize::from(has_duration)
        + usize::from(has_size)
        + usize::from(has_flags)
        + usize::from(has_cts))
        * 4;
    let total_bytes = sample_count
        .checked_mul(per_sample_bytes)
        .ok_or_else(|| DrmError::Decrypt("trun: sample run overflows usize".into()))?;
    if off + total_bytes > data.len() {
        return Err(DrmError::Decrypt("trun: truncated sample run".into()));
    }

    let mut sizes = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if has_duration {
            off += 4;
        }
        let size = if has_size {
            let s = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            off += 4;
            s
        } else {
            tfhd_default_sample_size.ok_or_else(|| {
                DrmError::Decrypt(
                    "trun has no per-sample size and tfhd has no default_sample_size".into(),
                )
            })?
        };
        if has_flags {
            off += 4;
        }
        if has_cts {
            off += 4;
        }
        sizes.push(size);
    }

    Ok(TrackRunInfo { data_offset, sizes })
}

#[derive(Debug, Clone)]
struct SampleEncEntry {
    iv: Vec<u8>,
    subsamples: Vec<(u16, u32)>, // (clear_bytes, encrypted_bytes)
}

fn parse_senc(
    data: &[u8],
    bref: BoxRef,
    default_iv_size: u8,
) -> Result<Vec<SampleEncEntry>, DrmError> {
    let (_, flags, body) = full_box_header(data, bref)
        .ok_or_else(|| DrmError::Decrypt("senc: truncated full-box header".into()))?;
    if body + 4 > data.len() {
        return Err(DrmError::Decrypt("senc: truncated sample_count".into()));
    }
    let sample_count =
        u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]]) as usize;
    let has_subsamples = flags & 0x0000_0002 != 0;
    let iv_size = usize::from(default_iv_size);

    let mut off = body + 4;
    let mut entries = Vec::with_capacity(sample_count);

    for _ in 0..sample_count {
        if off + iv_size > data.len() {
            return Err(DrmError::Decrypt("senc: truncated per-sample IV".into()));
        }
        let iv = data[off..off + iv_size].to_vec();
        off += iv_size;

        let subsamples = if has_subsamples {
            if off + 2 > data.len() {
                return Err(DrmError::Decrypt("senc: truncated subsample_count".into()));
            }
            let count = u16::from_be_bytes([data[off], data[off + 1]]) as usize;
            off += 2;
            let mut subs = Vec::with_capacity(count);
            for _ in 0..count {
                if off + 6 > data.len() {
                    return Err(DrmError::Decrypt("senc: truncated subsample entry".into()));
                }
                let clear = u16::from_be_bytes([data[off], data[off + 1]]);
                let enc = u32::from_be_bytes([
                    data[off + 2],
                    data[off + 3],
                    data[off + 4],
                    data[off + 5],
                ]);
                off += 6;
                subs.push((clear, enc));
            }
            subs
        } else {
            Vec::new()
        };

        entries.push(SampleEncEntry { iv, subsamples });
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Decrypts a CENC-encrypted MP4 in memory and returns the decrypted bytes.
///
/// The returned buffer has the protected sample entry (`enca`) rewritten to
/// `mp4a` and the protection sub-boxes (`sinf` inside the sample entry,
/// `senc` / `saiz` / `saio` inside each `traf`) renamed to `free` so players
/// treat the file as unencrypted. Fragment decode times are rebased so each
/// track starts at 0 ([`rebase_fragment_times`]).
///
/// Returns [`DrmError::Decrypt`] (variant message) for:
/// * missing or malformed `sinf` / `schm` / `tenc`;
/// * any scheme other than `cenc` or `cbcs`;
/// * a `moof` that contains `senc` but cannot be fully decrypted;
/// * malformed MP4 box sizes anywhere in the file.
pub fn decrypt(data: Vec<u8>, _key_id: &[u8], content_key: &[u8]) -> Result<Vec<u8>, DrmError> {
    // Take ownership so the AES-CTR / AES-CBC passes rewrite the original
    // buffer in place — one fewer full-file copy per song.
    let mut buf = data;

    // 1. Parse encryption metadata from moov/.../stsd/<sample_entry>/sinf.
    let (scheme_type, tenc) = read_encryption_params(&buf)?;

    // 2. Pick the cipher scheme. `cenc` is AES-CTR; `cbcs` is AES-CBC with
    //    sample-based pattern encryption (ISO/IEC 23001-7 clauses 10.1 / 10.4).
    let scheme = match &scheme_type {
        b"cenc" => Scheme::Cenc,
        b"cbcs" => Scheme::Cbcs,
        _ => {
            let name = String::from_utf8_lossy(&scheme_type).into_owned();
            return Err(DrmError::Decrypt(format!("unsupported scheme: {name}")));
        }
    };

    let key: &[u8; 16] = content_key
        .try_into()
        .map_err(|_| DrmError::Decrypt("content key must be 16 bytes".into()))?;

    // 3. Decrypt every `moof`'s encrypted traf in place. Any malformed metadata
    //    in an encrypted track becomes an error — no silent "oops still
    //    encrypted" output.
    let mut pos = 0;
    while pos < buf.len() {
        let Some((t, b)) = read_box(&buf, pos)? else {
            break;
        };
        if t == *b"moof" {
            decrypt_moof(&mut buf, b, &tenc, key, scheme)?;
        }
        pos = b.offset + b.size;
    }

    // 4. Neutralize protection markers: rewrite enca -> mp4a in stsd,
    //    rename sinf/senc/saiz/saio -> free purely through structural walks
    //    so byte sequences inside mdat are never touched.
    neutralize_protection(&mut buf)?;

    // 5. Start the media timeline at 0. Music-video audio keeps the video's
    //    start offset (about 10 s), which players would otherwise play as
    //    leading silence or show as a wrong duration.
    rebase_fragment_times(&mut buf)?;

    Ok(buf)
}

/// Shifts every `moof/traf/tfdt` so each track's first fragment starts at 0.
///
/// Apple's music-video audio renditions keep the video's media timeline (the
/// first `baseMediaDecodeTime` is ~10 s, not 0). Because `moov` declares no
/// duration, no `mehd` and no edit list, players then count that offset as
/// part of the track's length. Subtracting the first `tfdt` of each track
/// (keyed by `tfhd.track_ID`) from all of that track's `tfdt`s removes it.
///
/// Each value is written back in its own width (version 0: `u32`, version 1:
/// `u64`), so no box changes size and no offsets move. A file whose tracks
/// already start at 0 is left byte-for-byte unchanged. A later `tfdt` that is
/// smaller than its track's first one is reported as [`DrmError::Decrypt`].
fn rebase_fragment_times(buf: &mut [u8]) -> Result<(), DrmError> {
    let mut bases: Vec<(u32, u64)> = Vec::new();
    let mut pos = 0;
    while let Some((t, moof)) = read_box(buf, pos)? {
        pos = moof.offset + moof.size;
        if t != *b"moof" {
            continue;
        }
        let moof_end = moof.offset + moof.size;
        let mut traf_pos = moof.payload;
        while traf_pos < moof_end {
            let Some((t, traf)) = read_box(buf, traf_pos)? else {
                break;
            };
            traf_pos = traf.offset + traf.size;
            if t == *b"traf" {
                rebase_traf_time(buf, traf, &mut bases)?;
            }
        }
    }
    Ok(())
}

/// Rebases the `tfdt` of one `traf` against its track's first base time,
/// recording that base in `bases` when this is the track's first fragment.
fn rebase_traf_time(
    buf: &mut [u8],
    traf: BoxRef,
    bases: &mut Vec<(u32, u64)>,
) -> Result<(), DrmError> {
    let traf_end = traf.offset + traf.size;
    let Some(tfdt) = find_box(buf, traf.payload, traf_end, *b"tfdt")? else {
        return Ok(());
    };
    let tfhd = find_box(buf, traf.payload, traf_end, *b"tfhd")?
        .ok_or_else(|| DrmError::Decrypt("traf has tfdt but no tfhd".into()))?;
    let track_id = parse_tfhd(buf, tfhd)?.track_id;

    let (version, _, body) = full_box_header(buf, tfdt)
        .ok_or_else(|| DrmError::Decrypt("tfdt: truncated full-box header".into()))?;
    let width = if version == 1 { 8 } else { 4 };
    if body + width > tfdt.offset + tfdt.size {
        return Err(DrmError::Decrypt(
            "tfdt: truncated baseMediaDecodeTime".into(),
        ));
    }
    let field = &mut buf[body..body + width];

    // Big-endian, either width: fold the bytes into a u64.
    let time = field
        .iter()
        .fold(0u64, |acc, &byte| (acc << 8) | u64::from(byte));
    let base = if let Some(&(_, base)) = bases.iter().find(|(id, _)| *id == track_id) {
        base
    } else {
        bases.push((track_id, time));
        time
    };
    let rebased = time.checked_sub(base).ok_or_else(|| {
        DrmError::Decrypt(format!(
            "tfdt {time} of track {track_id} is before its first fragment ({base})"
        ))
    })?;

    if version == 1 {
        field.copy_from_slice(&rebased.to_be_bytes());
    } else {
        // `rebased <= time`, which came from a u32.
        let narrow = u32::try_from(rebased)
            .map_err(|_| DrmError::Decrypt("tfdt: rebased time exceeds u32".into()))?;
        field.copy_from_slice(&narrow.to_be_bytes());
    }
    Ok(())
}

/// Walks `moov` to find the `(scheme_type, tenc)` for the protected track.
fn read_encryption_params(buf: &[u8]) -> Result<([u8; 4], TrackEncryption), DrmError> {
    let stsd = find_box_path(
        buf,
        0,
        buf.len(),
        &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"],
    )?
    .ok_or_else(|| DrmError::Decrypt("missing moov/trak/mdia/minf/stbl/stsd".into()))?;

    // stsd is a full box: skip version/flags (4) + entry_count (4).
    if stsd.payload + 8 > stsd.offset + stsd.size {
        return Err(DrmError::Decrypt("stsd: truncated entry_count".into()));
    }
    let entries_start = stsd.payload + 8;
    let stsd_end = stsd.offset + stsd.size;

    // Walk sample entries looking for a protected one (`enca` or `encv`).
    let mut pos = entries_start;
    while pos < stsd_end {
        let Some((entry_type, entry)) = read_box(buf, pos)? else {
            break;
        };
        let prelude = match &entry_type {
            b"enca" => Some(AUDIO_SAMPLE_ENTRY_PRELUDE),
            b"encv" => Some(VISUAL_SAMPLE_ENTRY_PRELUDE),
            _ => None,
        };
        if let Some(prelude) = prelude {
            let child_start = entry
                .payload
                .checked_add(prelude)
                .ok_or_else(|| DrmError::Decrypt("sample entry prelude offset overflow".into()))?;
            let entry_end = entry.offset + entry.size;
            if child_start > entry_end {
                return Err(DrmError::Decrypt(
                    "sample entry smaller than its header prelude".into(),
                ));
            }
            if let Some(sinf) = find_box(buf, child_start, entry_end, *b"sinf")? {
                let sinf_end = sinf.offset + sinf.size;
                let schm = find_box(buf, sinf.payload, sinf_end, *b"schm")?
                    .ok_or_else(|| DrmError::Decrypt("sinf missing schm".into()))?;
                let scheme_type = parse_schm_scheme_type(buf, schm)?;
                let tenc_bref = find_box_path(buf, sinf.payload, sinf_end, &[*b"schi", *b"tenc"])?
                    .ok_or_else(|| DrmError::Decrypt("sinf missing schi/tenc".into()))?;
                let tenc = parse_tenc(buf, tenc_bref)?;
                return Ok((scheme_type, tenc));
            }
        }
        pos = entry.offset + entry.size;
    }

    Err(DrmError::Decrypt(
        "stsd has no protected sample entry with a sinf".into(),
    ))
}

/// Decrypts every encrypted `traf` inside this `moof`.
fn decrypt_moof(
    buf: &mut [u8],
    moof: BoxRef,
    tenc: &TrackEncryption,
    key: &[u8; 16],
    scheme: Scheme,
) -> Result<(), DrmError> {
    let moof_end = moof.offset + moof.size;
    let mut pos = moof.payload;
    while pos < moof_end {
        let Some((t, b)) = read_box(buf, pos)? else {
            break;
        };
        if t == *b"traf" {
            decrypt_traf(buf, moof, b, tenc, key, scheme)?;
        }
        pos = b.offset + b.size;
    }
    Ok(())
}

/// Decrypts a single `traf`, if it carries `senc` (i.e. is encrypted).
///
/// Follows "parse, don't validate": a `senc` present with any malformed /
/// missing companion box turns into [`DrmError::Decrypt`]. A `traf`
/// without `senc` is left alone (unencrypted fragment is fine).
fn decrypt_traf(
    buf: &mut [u8],
    moof: BoxRef,
    traf: BoxRef,
    tenc: &TrackEncryption,
    key: &[u8; 16],
    scheme: Scheme,
) -> Result<(), DrmError> {
    let traf_end = traf.offset + traf.size;

    let tfhd_bref = find_box(buf, traf.payload, traf_end, *b"tfhd")?
        .ok_or_else(|| DrmError::Decrypt("traf missing tfhd".into()))?;
    let tfhd = parse_tfhd(buf, tfhd_bref)?;

    let Some(senc_bref) = find_box(buf, traf.payload, traf_end, *b"senc")? else {
        // Unprotected fragment — nothing to do.
        return Ok(());
    };

    // Once senc is present, every downstream failure must bubble up.
    //
    // A `traf` may hold several `trun`s (Apple's music-video audio packs ~6 s
    // of AAC as four runs of ~70 samples). `senc` describes every sample of
    // the `traf` in order, so the runs are concatenated before pairing.
    let truns = collect_truns(buf, traf.payload, traf_end, tfhd.default_sample_size)?;
    if truns.is_empty() {
        return Err(DrmError::Decrypt("encrypted traf missing trun".into()));
    }
    let senc_entries = parse_senc(buf, senc_bref, tenc.default_per_sample_iv_size)?;

    let trun_sample_count: usize = truns.iter().map(|t| t.sizes.len()).sum();
    if senc_entries.len() != trun_sample_count {
        return Err(DrmError::Decrypt(format!(
            "senc/trun sample count mismatch: {} senc vs {} trun ({} trun boxes)",
            senc_entries.len(),
            trun_sample_count,
            truns.len(),
        )));
    }

    // cbcs uses the constant IV from tenc (per sample); it is required for
    // `default_per_sample_IV_size == 0`, which Apple's music videos use.
    let cbcs_iv = if matches!(scheme, Scheme::Cbcs) {
        Some(resolve_cbcs_iv(tenc, &senc_entries)?)
    } else {
        None
    };

    let samples = sample_spans(buf, moof, &tfhd, &truns)?;

    match scheme {
        Scheme::Cenc => decrypt_samples(buf, &samples, &senc_entries, key),
        Scheme::Cbcs => {
            let iv = cbcs_iv.ok_or_else(|| {
                DrmError::Decrypt("internal: cbcs_iv was not populated for Scheme::Cbcs".into())
            })?;
            let params = CbcsParams {
                key,
                constant_iv: &iv,
                crypt_byte_block: tenc.default_crypt_byte_block,
                skip_byte_block: tenc.default_skip_byte_block,
            };
            decrypt_samples_cbcs(buf, &samples, &senc_entries, &params)
        }
    }
}

/// Parses every `trun` directly inside `traf[start..end]`, in file order.
fn collect_truns(
    buf: &[u8],
    start: usize,
    end: usize,
    tfhd_default_sample_size: Option<u32>,
) -> Result<Vec<TrackRunInfo>, DrmError> {
    let mut truns = Vec::new();
    let mut pos = start;
    while pos < end {
        let Some((t, b)) = read_box(buf, pos)? else {
            break;
        };
        if b.offset + b.size > end {
            return Err(DrmError::Decrypt(format!(
                "box {} at {} extends past traf end {end}",
                String::from_utf8_lossy(&t),
                b.offset
            )));
        }
        if t == *b"trun" {
            truns.push(parse_trun(buf, b, tfhd_default_sample_size)?);
        }
        pos = b.offset + b.size;
    }
    Ok(truns)
}

/// Byte range of one sample inside the file buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SampleSpan {
    start: usize,
    end: usize,
}

/// Locates every sample of a `traf`'s runs, per ISO/IEC 14496-12 §8.8.7–8.8.8.
///
/// * The base offset is `tfhd.base_data_offset` when present, otherwise the
///   start of the enclosing `moof` (`default-base-is-moof`, and also the rule
///   for the first `traf` when neither flag is set).
/// * A run with `data_offset` starts at `base + data_offset`.
/// * A run without `data_offset` continues right after the previous run's
///   data. For the first run that has none, Apple's layout (the `mdat` payload
///   directly after the `moof`) is used.
fn sample_spans(
    buf: &[u8],
    moof: BoxRef,
    tfhd: &TrackFragmentHeader,
    truns: &[TrackRunInfo],
) -> Result<Vec<SampleSpan>, DrmError> {
    let base = match tfhd.base_data_offset {
        Some(base) => usize::try_from(base)
            .map_err(|_| DrmError::Decrypt("tfhd base_data_offset exceeds usize".into()))?,
        None => moof.offset,
    };

    let mut spans = Vec::with_capacity(truns.iter().map(|t| t.sizes.len()).sum());
    let mut next: Option<usize> = None;
    for trun in truns {
        let mut offset = match (trun.data_offset, next) {
            (Some(delta), _) => signed_add_usize(base, i64::from(delta))?,
            (None, Some(previous_end)) => previous_end,
            (None, None) if tfhd.base_data_offset.is_some() || tfhd.default_base_is_moof => base,
            (None, None) => mdat_payload_after(buf, moof)?,
        };
        for (i, &size) in trun.sizes.iter().enumerate() {
            let end = offset
                .checked_add(size as usize)
                .ok_or_else(|| DrmError::Decrypt("sample offset overflow".into()))?;
            if end > buf.len() {
                return Err(DrmError::Decrypt(format!(
                    "sample {i} ends at {end} past buffer {}",
                    buf.len()
                )));
            }
            spans.push(SampleSpan { start: offset, end });
            offset = end;
        }
        next = Some(offset);
    }
    Ok(spans)
}

/// Payload offset of the `mdat` that immediately follows `moof`.
fn mdat_payload_after(buf: &[u8], moof: BoxRef) -> Result<usize, DrmError> {
    let after_moof = moof.offset + moof.size;
    let Some((mt, mdat)) = read_box(buf, after_moof)? else {
        return Err(DrmError::Decrypt(
            "expected mdat after moof, got EOF".into(),
        ));
    };
    if mt != *b"mdat" {
        return Err(DrmError::Decrypt(format!(
            "expected mdat after moof, got {}",
            String::from_utf8_lossy(&mt)
        )));
    }
    Ok(mdat.payload)
}

/// Keys and pattern parameters for cbcs sample decryption, so
/// `decrypt_samples_cbcs` stays within clippy's `too_many_arguments` limit and
/// its call sites read as one coherent group.
#[derive(Debug, Clone, Copy)]
struct CbcsParams<'a> {
    key: &'a [u8; 16],
    constant_iv: &'a [u8; 16],
    crypt_byte_block: u8,
    skip_byte_block: u8,
}

/// Widens an 8- or 16-byte IV to an AES block (ISO/IEC 23001-7 §9.1: an
/// 8-byte IV is zero-extended). Any other length yields `None`.
fn widen_iv(iv: &[u8]) -> Option<[u8; 16]> {
    matches!(iv.len(), 8 | 16).then(|| {
        let mut out = [0u8; 16];
        out[..iv.len()].copy_from_slice(iv);
        out
    })
}

/// Picks the 16-byte IV a cbcs sample starts from when its `senc` entry
/// carries no IV of its own.
///
/// Per ISO/IEC 23001-7 clause 10.4, cbcs normally carries the IV once in
/// `tenc.default_constant_IV` (with `default_per_sample_IV_size == 0`) rather
/// than per sample in `senc`. If a producer instead sets `per_sample_IV_size`
/// and lets every entry carry the same IV we accept the first entry's IV so
/// decryption still works (uniformly `default_per_sample_IV_size == 0` is what
/// Apple ships).
fn resolve_cbcs_iv(
    tenc: &TrackEncryption,
    senc_entries: &[SampleEncEntry],
) -> Result<[u8; 16], DrmError> {
    if let Some(iv) = tenc.default_constant_iv.as_deref().and_then(widen_iv) {
        return Ok(iv);
    }
    if let Some(iv) = senc_entries.first().and_then(|e| widen_iv(&e.iv)) {
        return Ok(iv);
    }
    Err(DrmError::Decrypt(
        "cbcs: no 8- or 16-byte IV (tenc default_constant_IV absent and senc carries none)".into(),
    ))
}

/// Decrypts samples under the `cbcs` scheme.
///
/// Per ISO/IEC 23001-7 clause 10.4:
/// * the AES-128-CBC IV is reset to `constant_iv` (or the sample's own `senc`
///   IV, when present) at the start of every sample and every subsample;
/// * within each sample's protected bytes the pattern
///   `crypt_byte_block : skip_byte_block` repeats: `crypt_byte_block` 16-byte
///   AES-CBC blocks are decrypted, then `skip_byte_block` blocks are left in
///   the clear, and so on. The CBC chain continues across skipped blocks (the
///   "previous ciphertext" fed to the next decrypt stays the ciphertext of the
///   LAST block of the preceding crypt run — skipped-block bytes do not reset
///   it). When `crypt_byte_block == 0 && skip_byte_block == 0`, every complete
///   16-byte block of the protected range is encrypted (cenc-like, no pattern).
/// * the trailing `len % 16` bytes of a protected range are always clear.
fn decrypt_samples_cbcs(
    buf: &mut [u8],
    samples: &[SampleSpan],
    senc_entries: &[SampleEncEntry],
    params: &CbcsParams<'_>,
) -> Result<(), DrmError> {
    let cipher = Aes128::new(params.key.into());
    let constant_iv = params.constant_iv;
    let crypt_byte_block = params.crypt_byte_block;
    let skip_byte_block = params.skip_byte_block;
    for (i, span) in samples.iter().enumerate() {
        let (offset, sample_end) = checked_span(buf, i, *span)?;
        let Some(entry) = senc_entries.get(i) else {
            return Err(DrmError::Decrypt(format!(
                "missing senc entry for sample {i}"
            )));
        };

        // A per-sample IV in `senc` (rare for cbcs) overrides the constant IV.
        let sample_iv = widen_iv(&entry.iv).unwrap_or(*constant_iv);

        if entry.subsamples.is_empty() {
            // Whole sample is the protected range; IV resets at the sample.
            let mut iv = sample_iv;
            decrypt_cbcs_range(
                &cipher,
                &mut buf[offset..sample_end],
                &mut iv,
                crypt_byte_block,
                skip_byte_block,
            );
        } else {
            let mut pos = offset;
            for &(clear, encrypted) in &entry.subsamples {
                pos = pos
                    .checked_add(clear as usize)
                    .ok_or_else(|| DrmError::Decrypt("subsample clear overflow".into()))?;
                let enc_end = pos
                    .checked_add(encrypted as usize)
                    .ok_or_else(|| DrmError::Decrypt("subsample encrypted overflow".into()))?;
                if enc_end > sample_end {
                    return Err(DrmError::Decrypt("subsample overflows sample".into()));
                }
                // IV resets at the start of each subsample's protected bytes,
                // per ISO/IEC 23001-7 cbcs (the chain continues within the
                // protected range — including across skipped blocks — but not
                // across the clear bytes between subsamples).
                let mut iv = sample_iv;
                decrypt_cbcs_range(
                    &cipher,
                    &mut buf[pos..enc_end],
                    &mut iv,
                    crypt_byte_block,
                    skip_byte_block,
                );
                pos = enc_end;
            }
        }
    }
    Ok(())
}

/// Decrypts one protected byte range in place under cbcs pattern encryption.
///
/// `iv` is the running CBC chain input. It is updated after every decrypted
/// block (both crypt-pattern blocks and the "every block" case when the
/// pattern is `0:0`) so the chain state survives across subsamples of the same
/// sample. Skipped blocks are left unchanged but DO continue the chain — they
/// do so by updating `iv` to the LAST decrypted block's ciphertext from the
/// crypt run, which `iv` already holds. In other words: skipped blocks do not
/// touch `iv` because they are not ciphertext to decrypt; the next crypt run
/// chains from the previous crypt run's last ciphertext block.
fn decrypt_cbcs_range(
    cipher: &Aes128,
    range: &mut [u8],
    iv: &mut [u8; 16],
    crypt_byte_block: u8,
    skip_byte_block: u8,
) {
    // Pattern 0:0 means "no skipping": every complete block is encrypted.
    let (cb, sb) = if crypt_byte_block == 0 && skip_byte_block == 0 {
        (u32::MAX, 0u32)
    } else {
        (u32::from(crypt_byte_block), u32::from(skip_byte_block))
    };

    let block_count = range.len() / 16;
    let mut block_idx = 0usize;
    while block_idx < block_count {
        let remaining = u32::try_from(block_count - block_idx).unwrap_or(u32::MAX);
        let crypt_now = cb.min(remaining);
        for _ in 0..crypt_now {
            let start = block_idx * 16;
            let end = start + 16;
            // `ct` is the current ciphertext block; run it through the
            // block cipher (raw decrypt), then XOR with the prior CBC input
            // (`iv`) to recover the plaintext. The next crypt block chains
            // from THIS block's original ciphertext (`ct`).
            let mut ct = [0u8; 16];
            ct.copy_from_slice(&range[start..end]);
            // `Block = Array<u8, U16>` implements `From<[u8; 16]>` and `From<&[u8; 16]>`;
            // we clone the current ciphertext bytes into it, run the block
            // cipher, then XOR with `iv` to recover the plaintext.
            let mut block: Block = ct.into();
            cipher.decrypt_block(&mut block);
            for j in 0..16 {
                range[start + j] = block[j] ^ iv[j];
            }
            *iv = ct;
            block_idx += 1;
        }
        // `sb == 0` means the pattern is "all-encrypt" (the 0:0 special-case
        // fell through to cb=MAX, sb=0 above); nothing left to skip.
        if sb == 0 {
            break;
        }
        // Skip `sb` blocks — plaintext in the clear. The chain input (`iv`)
        // already carries the previous crypt block's ciphertext forward.
        let remaining_after_crypt = u32::try_from(block_count - block_idx).unwrap_or(u32::MAX);
        let skip_now = sb.min(remaining_after_crypt);
        block_idx += skip_now as usize;
    }
    // Trailing `range.len() % 16` bytes are left clear per spec.
}

/// Validates a sample span against the buffer, returning `(start, end)`.
fn checked_span(buf: &[u8], index: usize, span: SampleSpan) -> Result<(usize, usize), DrmError> {
    if span.start > span.end || span.end > buf.len() {
        return Err(DrmError::Decrypt(format!(
            "sample {index} range {}..{} outside buffer of {} bytes",
            span.start,
            span.end,
            buf.len()
        )));
    }
    Ok((span.start, span.end))
}

fn signed_add_usize(base: usize, delta: i64) -> Result<usize, DrmError> {
    let base_i64 = i64::try_from(base)
        .map_err(|_| DrmError::Decrypt("sample data base exceeds i64".into()))?;
    let result = base_i64
        .checked_add(delta)
        .ok_or_else(|| DrmError::Decrypt("sample data offset overflow".into()))?;
    usize::try_from(result)
        .map_err(|_| DrmError::Decrypt("sample data offset negative or overflow".into()))
}

/// Decrypts the sample data starting at `data_start`, in place.
fn decrypt_samples(
    buf: &mut [u8],
    samples: &[SampleSpan],
    senc_entries: &[SampleEncEntry],
    key: &[u8; 16],
) -> Result<(), DrmError> {
    for (i, span) in samples.iter().enumerate() {
        let (offset, sample_end) = checked_span(buf, i, *span)?;

        // Every span comes with an entry (checked upstream via the count
        // match); this guards direct callers.
        let Some(entry) = senc_entries.get(i) else {
            return Err(DrmError::Decrypt(format!(
                "missing senc entry for sample {i}"
            )));
        };

        // Build the 16-byte CTR counter: pad the per-sample IV with zeros.
        let mut iv_block = [0u8; 16];
        let copy_len = entry.iv.len().min(16);
        iv_block[..copy_len].copy_from_slice(&entry.iv[..copy_len]);

        if entry.subsamples.is_empty() {
            // Full-sample encryption.
            let mut cipher = Aes128Ctr::new(key.into(), (&iv_block).into());
            cipher.apply_keystream(&mut buf[offset..sample_end]);
        } else {
            let mut pos = offset;
            let mut cipher = Aes128Ctr::new(key.into(), (&iv_block).into());
            for &(clear, encrypted) in &entry.subsamples {
                pos = pos
                    .checked_add(clear as usize)
                    .ok_or_else(|| DrmError::Decrypt("subsample clear overflow".into()))?;
                let enc_end = pos
                    .checked_add(encrypted as usize)
                    .ok_or_else(|| DrmError::Decrypt("subsample encrypted overflow".into()))?;
                if enc_end > sample_end {
                    return Err(DrmError::Decrypt("subsample overflows sample".into()));
                }
                cipher.apply_keystream(&mut buf[pos..enc_end]);
                pos = enc_end;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Structural renaming
// ---------------------------------------------------------------------------

/// Rewrites protection markers using nested-box walks.
///
/// * `moov/trak/mdia/minf/stbl/stsd/enca` → `mp4a`, plus its child `sinf` →
///   `free`.
/// * `moov/trak/mdia/minf/stbl/stsd/encv` → `mp4v` + child `sinf` → `free`
///   (for future music-video support; harmless on audio-only files).
/// * Every `moof/traf/{senc,saiz,saio}` → `free`.
/// * `moov/pssh` and `moof/pssh` → `free`.
///
/// Only sample-entry types and traf children are touched — never any bytes
/// inside `mdat`, where a byte scan could hit audio data that happens to
/// spell a box name.
fn neutralize_protection(buf: &mut [u8]) -> Result<(), DrmError> {
    // --- moov/.../stsd/<sample_entry> ---------------------------------------
    if let Some(stsd) = find_box_path(
        buf,
        0,
        buf.len(),
        &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"],
    )? {
        let stsd_end = stsd.offset + stsd.size;
        if stsd.payload + 8 <= stsd_end {
            let entries_start = stsd.payload + 8;
            let mut pos = entries_start;
            while pos < stsd_end {
                let Some((entry_type, entry)) = read_box(buf, pos)? else {
                    break;
                };
                let (replacement, prelude) = match &entry_type {
                    b"enca" => (Some(*b"mp4a"), Some(AUDIO_SAMPLE_ENTRY_PRELUDE)),
                    b"encv" => (Some(*b"mp4v"), Some(VISUAL_SAMPLE_ENTRY_PRELUDE)),
                    _ => (None, None),
                };
                if let (Some(replacement), Some(prelude)) = (replacement, prelude) {
                    // Rewrite the type field of the sample entry.
                    buf[entry.offset + 4..entry.offset + 8].copy_from_slice(&replacement);
                    let child_start = entry.payload + prelude;
                    let entry_end = entry.offset + entry.size;
                    if child_start <= entry_end {
                        rename_boxes_in_region(buf, child_start, entry_end, &[*b"sinf"], *b"free")?;
                    }
                }
                pos = entry.offset + entry.size;
            }
        }
    }

    // --- moov/pssh, moof/pssh, moof/traf --------------------------------------
    let mut pos = 0;
    while pos < buf.len() {
        let Some((t, b)) = read_box(buf, pos)? else {
            break;
        };
        if t == *b"moov" || t == *b"moof" {
            // DRM system headers are meaningless once the samples are clear.
            rename_boxes_in_region(buf, b.payload, b.offset + b.size, &[*b"pssh"], *b"free")?;
        }
        if t == *b"moof" {
            let moof_end = b.offset + b.size;
            let mut tp = b.payload;
            while tp < moof_end {
                let Some((tt, tb)) = read_box(buf, tp)? else {
                    break;
                };
                if tt == *b"traf" {
                    let traf_end = tb.offset + tb.size;
                    rename_boxes_in_region(
                        buf,
                        tb.payload,
                        traf_end,
                        &[*b"senc", *b"saiz", *b"saio"],
                        *b"free",
                    )?;
                }
                tp = tb.offset + tb.size;
            }
        }
        pos = b.offset + b.size;
    }

    Ok(())
}

fn rename_boxes_in_region(
    buf: &mut [u8],
    start: usize,
    end: usize,
    targets: &[[u8; 4]],
    replacement: [u8; 4],
) -> Result<(), DrmError> {
    let mut pos = start;
    while pos < end {
        let Some((t, b)) = read_box(buf, pos)? else {
            break;
        };
        if targets.contains(&t) {
            buf[b.offset + 4..b.offset + 8].copy_from_slice(&replacement);
        }
        pos = b.offset + b.size;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- box builders ------------------------------------------------------

    /// Build `size + type + payload` with a regular 32-bit size.
    fn mk_box(ty: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).expect("box size fits in u32");
        let mut v = Vec::with_capacity(8 + payload.len());
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&ty);
        v.extend_from_slice(payload);
        v
    }

    /// Build a `FullBox` (version + flags header, then payload).
    fn mk_full_box(ty: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(4 + payload.len());
        body.push(version);
        body.push(u8::try_from((flags >> 16) & 0xFF).expect("fits"));
        body.push(u8::try_from((flags >> 8) & 0xFF).expect("fits"));
        body.push(u8::try_from(flags & 0xFF).expect("fits"));
        body.extend_from_slice(payload);
        mk_box(ty, &body)
    }

    fn concat_bytes(parts: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
        }
        v
    }

    /// Construct `stsd` with a single `enca` sample entry that contains
    /// `sinf/schm` + `sinf/schi/tenc`. `scheme` is the 4CC written into schm.
    #[expect(
        clippy::similar_names,
        reason = "schi, schm and sinf are the spec's own four-letter box codes"
    )]
    fn mk_stsd(scheme: [u8; 4], iv_size: u8) -> Vec<u8> {
        // tenc v0 body (post FullBox): reserved(1) reserved(1) is_protected(1)
        //                             iv_size(1) kid(16) = 20 bytes.
        let mut tenc_body = vec![
            0,       // reserved
            0,       // reserved
            1,       // default_is_protected
            iv_size, // default_Per_Sample_IV_Size
        ];
        tenc_body.extend_from_slice(&[0u8; 16]); // KID
        let tenc = mk_full_box(*b"tenc", 0, 0, &tenc_body);
        let schi = mk_box(*b"schi", &tenc);

        // schm body: scheme_type(4) + scheme_version(4). No URI (flags=0).
        let schm_payload = concat_bytes(&[&scheme, &0x0001_0000u32.to_be_bytes()]);
        let schm = mk_full_box(*b"schm", 0, 0, &schm_payload);

        // frma (optional but realistic): original_format `mp4a`.
        let frma = mk_box(*b"frma", b"mp4a");

        let sinf_children = concat_bytes(&[&frma, &schm, &schi]);
        let sinf = mk_box(*b"sinf", &sinf_children);

        // AudioSampleEntry prelude: 28 bytes (6 reserved + 2 dref_idx + 8 reserved
        // + 2 channelcount + 2 samplesize + 2 pre_defined + 2 reserved + 4 samplerate).
        let prelude = [0u8; AUDIO_SAMPLE_ENTRY_PRELUDE];
        let enca_payload = concat_bytes(&[&prelude, &sinf]);
        let enca = mk_box(*b"enca", &enca_payload);

        // stsd body: version/flags(4) + entry_count(4=1) + entries.
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        stsd_body.extend_from_slice(&enca);
        mk_full_box(*b"stsd", 0, 0, &stsd_body)
    }

    /// Wrap a `stsd` payload in the nested moov/trak/mdia/minf/stbl chain.
    fn mk_moov_with_stsd(stsd: &[u8]) -> Vec<u8> {
        let stbl = mk_box(*b"stbl", stsd);
        let minf = mk_box(*b"minf", &stbl);
        let mdia = mk_box(*b"mdia", &minf);
        let trak = mk_box(*b"trak", &mdia);
        mk_box(*b"moov", &trak)
    }

    /// Returns the offset of the first occurrence of `needle` in `hay`.
    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    // ---- read_box ----------------------------------------------------------

    #[test]
    fn read_box_parses_basic_box() {
        let mut data = Vec::new();
        data.extend_from_slice(&16u32.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(&[0u8; 8]);
        let (btype, bref) = read_box(&data, 0).unwrap().unwrap();
        assert_eq!(&btype, b"ftyp");
        assert_eq!(bref.size, 16);
        assert_eq!(bref.payload, 8);
    }

    #[test]
    fn read_box_size_zero_means_rest_of_buffer() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_be_bytes()); // size = 0
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&[42u8; 12]); // trailing bytes
        let (btype, bref) = read_box(&data, 0).unwrap().unwrap();
        assert_eq!(&btype, b"mdat");
        assert_eq!(bref.size, 20); // header (8) + 12 trailing
        assert_eq!(bref.payload, 8);
    }

    #[test]
    fn read_box_supports_largesize_box() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // size marker = 1
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&24u64.to_be_bytes()); // 64-bit size
        data.extend_from_slice(&[7u8; 8]); // payload
        let (btype, bref) = read_box(&data, 0).unwrap().unwrap();
        assert_eq!(&btype, b"mdat");
        assert_eq!(bref.size, 24);
        assert_eq!(bref.payload, 16);
    }

    #[test]
    fn read_box_rejects_largesize_smaller_than_header() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&8u64.to_be_bytes()); // declared < 16
        data.extend_from_slice(&[0u8; 8]);
        assert!(matches!(read_box(&data, 0), Err(DrmError::Decrypt(_))));
    }

    #[test]
    fn read_box_rejects_size_smaller_than_header() {
        for bad_size in [2u32, 3, 7] {
            let mut data = Vec::new();
            data.extend_from_slice(&bad_size.to_be_bytes());
            data.extend_from_slice(b"ftyp");
            data.extend_from_slice(&[0u8; 8]);
            let err = read_box(&data, 0);
            assert!(
                matches!(err, Err(DrmError::Decrypt(_))),
                "expected error for size {bad_size}, got {err:?}"
            );
        }
    }

    #[test]
    fn read_box_rejects_size_past_buffer() {
        let mut data = Vec::new();
        data.extend_from_slice(&32u32.to_be_bytes()); // claims 32 bytes
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(&[0u8; 4]); // but only 16 bytes total
        assert!(matches!(read_box(&data, 0), Err(DrmError::Decrypt(_))));
    }

    #[test]
    fn read_box_offset_at_end_returns_none() {
        let data = mk_box(*b"ftyp", &[0u8; 8]);
        let after = read_box(&data, data.len()).unwrap();
        assert!(after.is_none(), "offset == len should return Ok(None)");
    }

    // ---- tenc --------------------------------------------------------------

    #[test]
    fn parse_tenc_v0_reads_iv_size_and_kid() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 1, 8]); // reserved, reserved, is_protected=1, iv_size=8
        body.extend_from_slice(&[0xAAu8; 16]); // KID
        let tenc_box = mk_full_box(*b"tenc", 0, 0, &body);
        let (_, bref) = read_box(&tenc_box, 0).unwrap().unwrap();
        let tenc = parse_tenc(&tenc_box, bref).unwrap();
        assert_eq!(tenc.default_is_protected, 1);
        assert_eq!(tenc.default_per_sample_iv_size, 8);
        assert_eq!(tenc.default_kid, [0xAAu8; 16]);
        assert_eq!(tenc.default_crypt_byte_block, 0);
        assert_eq!(tenc.default_skip_byte_block, 0);
        assert!(tenc.default_constant_iv.is_none());
    }

    #[test]
    fn parse_tenc_v1_reads_crypt_skip_blocks_and_constant_iv() {
        let mut body = vec![
            0,                // reserved
            (1u8 << 4) | 9u8, // crypt=1, skip=9
            1,                // is_protected
            0,                // per-sample iv_size = 0 → constant IV follows KID
        ];
        body.extend_from_slice(&[0xBBu8; 16]); // KID
        body.push(16); // constant_IV_size
        body.extend_from_slice(&[0xCCu8; 16]); // constant_IV
        let tenc_box = mk_full_box(*b"tenc", 1, 0, &body);
        let (_, bref) = read_box(&tenc_box, 0).unwrap().unwrap();
        let tenc = parse_tenc(&tenc_box, bref).unwrap();
        assert_eq!(tenc.default_crypt_byte_block, 1);
        assert_eq!(tenc.default_skip_byte_block, 9);
        assert_eq!(tenc.default_per_sample_iv_size, 0);
        assert_eq!(tenc.default_constant_iv.as_deref(), Some(&[0xCCu8; 16][..]));
    }

    // ---- decrypt: scheme checks -------------------------------------------

    #[test]
    fn decrypt_rejects_unsupported_scheme_cens() {
        let stsd = mk_stsd(*b"cens", 8);
        let moov = mk_moov_with_stsd(&stsd);
        let err = decrypt(moov, &[], &[0u8; 16]);
        match err {
            Err(DrmError::Decrypt(msg)) => assert!(msg.contains("cens"), "got: {msg}"),
            other => panic!("expected Decrypt error, got {other:?}"),
        }
    }

    #[test]
    fn decrypt_errors_when_schm_is_missing() {
        // Hand-build stsd/enca/sinf WITHOUT schm.
        let mut tenc_body = Vec::new();
        tenc_body.extend_from_slice(&[0, 0, 1, 8]);
        tenc_body.extend_from_slice(&[0u8; 16]);
        let tenc = mk_full_box(*b"tenc", 0, 0, &tenc_body);
        let schi = mk_box(*b"schi", &tenc);
        let sinf = mk_box(*b"sinf", &schi); // no schm
        let prelude = [0u8; AUDIO_SAMPLE_ENTRY_PRELUDE];
        let enca_payload = concat_bytes(&[&prelude, &sinf]);
        let enca = mk_box(*b"enca", &enca_payload);
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&1u32.to_be_bytes());
        stsd_body.extend_from_slice(&enca);
        let stsd = mk_full_box(*b"stsd", 0, 0, &stsd_body);
        let moov = mk_moov_with_stsd(&stsd);
        match decrypt(moov, &[], &[0u8; 16]) {
            Err(DrmError::Decrypt(msg)) => assert!(msg.contains("schm"), "got: {msg}"),
            other => panic!("expected Decrypt(missing schm), got {other:?}"),
        }
    }

    // ---- enca → mp4a rewrite + sinf → free ---------------------------------

    #[test]
    fn rewrites_enca_to_mp4a_in_nested_stsd() {
        let stsd = mk_stsd(*b"cenc", 8);
        let moov = mk_moov_with_stsd(&stsd);
        let mut buf = moov.clone();

        // Record where the `enca` box lives before rewriting. (The `sinf`
        // child already contains an `frma` whose payload is `mp4a`, so a bare
        // substring search cannot tell "box type was flipped" from "payload
        // happened to contain mp4a" — pinning the offset first does.)
        let enca_offset = find_subslice(&buf, b"enca").expect("enca present before rewrite");

        neutralize_protection(&mut buf).unwrap();

        assert_eq!(
            &buf[enca_offset..enca_offset + 4],
            b"mp4a",
            "the 4CC at the sample-entry's type offset flipped from enca to mp4a"
        );
        // The buffer must no longer announce an `enca` sample-entry type.
        // (`frma` also uses the `enca` original_format, but our mk_stsd
        // builder writes `mp4a` into `frma`, so no stray `enca` should remain.)
        assert!(
            find_subslice(&buf, b"enca").is_none(),
            "no enca bytes remain"
        );
    }

    #[test]
    fn rewrites_sinf_to_free_structurally() {
        let stsd = mk_stsd(*b"cenc", 8);
        let moov = mk_moov_with_stsd(&stsd);
        let mut buf = moov.clone();

        let sinf_offset = find_subslice(&moov, b"sinf").unwrap();
        neutralize_protection(&mut buf).unwrap();
        assert_eq!(&buf[sinf_offset..sinf_offset + 4], b"free");
    }

    #[test]
    fn neutralize_does_not_touch_mdat_payload_bytes() {
        // Build a file whose mdat happens to contain the ASCII bytes "sinf".
        let stsd = mk_stsd(*b"cenc", 8);
        let moov = mk_moov_with_stsd(&stsd);

        // mdat with "sinf" sitting inside the payload (not as a box header).
        let mdat_payload = b"xxxx sinf is only a byte sequence here".to_vec();
        let mdat = mk_box(*b"mdat", &mdat_payload);
        let mut file = concat_bytes(&[&moov, &mdat]);

        // Record the exact mdat payload bytes before the rewrite.
        let mdat_offset = find_subslice(&file, b"mdat").unwrap();
        let mdat_payload_start = mdat_offset + 4;
        let mdat_payload_end = file.len();
        let before: Vec<u8> = file[mdat_payload_start..mdat_payload_end].to_vec();

        neutralize_protection(&mut file).unwrap();

        // mdat's payload (the ASCII "sinf" included) is byte-for-byte unchanged.
        assert_eq!(&file[mdat_payload_start..mdat_payload_end], &before[..]);
    }

    // ---- tfhd / trun ------------------------------------------------------

    #[test]
    fn parse_trun_falls_back_to_tfhd_default_sample_size() {
        // trun flags=0x1 (data_offset), sample_count=3, no per-sample size.
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&64i32.to_be_bytes()); // data_offset
        let trun_box = mk_full_box(*b"trun", 0, 0x0000_0001, &body);
        let (_, bref) = read_box(&trun_box, 0).unwrap().unwrap();
        let info = parse_trun(&trun_box, bref, Some(100)).unwrap();
        assert_eq!(info.data_offset, Some(64));
        assert_eq!(info.sizes, vec![100, 100, 100]);
    }

    #[test]
    fn parse_trun_errors_when_no_per_sample_size_and_no_tfhd_default() {
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_be_bytes());
        let trun_box = mk_full_box(*b"trun", 0, 0, &body);
        let (_, bref) = read_box(&trun_box, 0).unwrap().unwrap();
        assert!(matches!(
            parse_trun(&trun_box, bref, None),
            Err(DrmError::Decrypt(_))
        ));
    }

    #[test]
    fn parse_tfhd_reads_default_base_is_moof_and_default_sample_size() {
        // flags: 0x10 (default_sample_size) | 0x20000 (default-base-is-moof)
        let flags = 0x0002_0010;
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes()); // track_id
        body.extend_from_slice(&999u32.to_be_bytes()); // default_sample_size
        let tfhd_box = mk_full_box(*b"tfhd", 0, flags, &body);
        let (_, bref) = read_box(&tfhd_box, 0).unwrap().unwrap();
        let info = parse_tfhd(&tfhd_box, bref).unwrap();
        assert_eq!(info.track_id, 1);
        assert_eq!(info.default_sample_size, Some(999));
        assert!(info.default_base_is_moof);
        assert_eq!(info.base_data_offset, None);
    }

    // ---- end-to-end via decrypt_traf-alike paths ---------------------------

    /// Builds a complete minimal MP4 file with one moof+mdat and runs decrypt.
    ///
    /// The plaintext in each sample is `plaintexts[i]`, encrypted with
    /// `key`/`iv_full` (CTR); senc holds the same `iv` (first 8 bytes of
    /// `iv_full`). The returned bytes are the full file after `decrypt`.
    fn build_cenc_file(
        key: [u8; 16],
        iv_prefix: [u8; 8],
        plaintexts: &[&[u8]],
        default_base_is_moof: bool,
        put_size_in_tfhd: bool,
        omit_trun: bool,
    ) -> Vec<u8> {
        use ctr::cipher::{KeyIvInit, StreamCipher};

        fn u32be_from_len(n: usize) -> [u8; 4] {
            u32::try_from(n)
                .expect("sample length fits in u32 in tests")
                .to_be_bytes()
        }

        // Encrypt plaintexts to form the mdat payload.
        let mut iv_full = [0u8; 16];
        iv_full[..8].copy_from_slice(&iv_prefix);
        let mut mdat_payload = Vec::new();
        let mut cipher_sample_ivs = Vec::new();
        for pt in plaintexts {
            let mut enc = pt.to_vec();
            let mut c = Aes128Ctr::new((&key).into(), (&iv_full).into());
            c.apply_keystream(&mut enc);
            cipher_sample_ivs.push(iv_prefix.to_vec());
            mdat_payload.extend_from_slice(&enc);
        }
        let mdat = mk_box(*b"mdat", &mdat_payload);

        // tfhd
        let mut tfhd_body = Vec::new();
        tfhd_body.extend_from_slice(&1u32.to_be_bytes()); // track_id
        let mut tfhd_flags = 0u32;
        if default_base_is_moof {
            tfhd_flags |= 0x0002_0000;
        }
        if put_size_in_tfhd {
            tfhd_flags |= 0x0000_0010;
            tfhd_body.extend_from_slice(&u32be_from_len(plaintexts[0].len()));
        }
        let tfhd = mk_full_box(*b"tfhd", 0, tfhd_flags, &tfhd_body);

        // trun — compute data_offset after we know moof size.
        let trun_flags: u32 = if put_size_in_tfhd {
            0x0000_0001 // data_offset only
        } else {
            0x0000_0001 | 0x0000_0200 // + per-sample size
        };

        let mut trun_body_without_offset = Vec::new();
        trun_body_without_offset.extend_from_slice(&u32be_from_len(plaintexts.len())); // sample_count

        // We'll append data_offset and sizes after we know moof size.

        // senc
        let mut senc_body = Vec::new();
        senc_body.extend_from_slice(&u32be_from_len(plaintexts.len())); // sample_count
        for iv in &cipher_sample_ivs {
            senc_body.extend_from_slice(iv);
        }
        let senc = mk_full_box(*b"senc", 0, 0, &senc_body);

        // Assemble moof with placeholder data_offset=0, measure its size,
        // then fix up data_offset to point at mdat payload.
        let moof_builder = |data_offset: i32| -> Vec<u8> {
            let mut trun_body = trun_body_without_offset.clone();
            trun_body.extend_from_slice(&data_offset.to_be_bytes());
            if !put_size_in_tfhd {
                for pt in plaintexts {
                    trun_body.extend_from_slice(&u32be_from_len(pt.len()));
                }
            }
            let trun = mk_full_box(*b"trun", 0, trun_flags, &trun_body);

            let mut traf_children = Vec::new();
            traf_children.extend_from_slice(&tfhd);
            if !omit_trun {
                traf_children.extend_from_slice(&trun);
            }
            traf_children.extend_from_slice(&senc);
            let traf = mk_box(*b"traf", &traf_children);

            // mfhd (mandatory in moof before traf).
            let mfhd_body = 1u32.to_be_bytes(); // sequence_number
            let mfhd = mk_full_box(*b"mfhd", 0, 0, &mfhd_body);

            let mut moof_children = Vec::new();
            moof_children.extend_from_slice(&mfhd);
            moof_children.extend_from_slice(&traf);
            mk_box(*b"moof", &moof_children)
        };
        let provisional = moof_builder(0);
        // Final moof is the same size regardless of the data_offset value, so
        // its length equals `provisional.len()`.
        let moof_size = provisional.len();

        // Build stsd/moov so we can find the final file layout.
        let stsd = mk_stsd(*b"cenc", 8);
        let moov = mk_moov_with_stsd(&stsd);
        let ftyp = mk_box(*b"ftyp", b"iso5\0\0\0\0mp42");
        let moov_end_in_file = ftyp.len() + moov.len();
        let moof_start_in_file = moov_end_in_file;
        let mdat_start_in_file = moof_start_in_file + moof_size;
        let mdat_payload_start_in_file = mdat_start_in_file + 8; // after mdat header

        let data_offset = if default_base_is_moof {
            // Relative to moof start.
            i32::try_from(mdat_payload_start_in_file - moof_start_in_file).unwrap()
        } else {
            // Not used in signed_add path; still set something sane.
            0
        };

        let moof_final = moof_builder(data_offset);
        assert_eq!(moof_final.len(), moof_size);

        concat_bytes(&[&ftyp, &moov, &moof_final, &mdat])
    }

    #[test]
    fn decrypt_uses_tfhd_default_sample_size_when_trun_omits_sizes() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 8];
        let sample = b"abcdefgh"; // 8 bytes
        let file = build_cenc_file(key, iv, &[sample], true, true, false);
        let out = decrypt(file, &[], &key).unwrap();

        // Locate mdat in the output and verify its payload is the plaintext.
        let mdat_offset = find_subslice(&out, b"mp4a").is_some().then(|| {
            // Find the box whose type is now "mdat". Search by scanning read_box.
            let mut pos = 0;
            while pos < out.len() {
                let (t, b) = read_box(&out, pos).unwrap().unwrap();
                if &t == b"mdat" {
                    return b.payload;
                }
                pos = b.offset + b.size;
            }
            panic!("mdat not found");
        });
        let mdat_payload = mdat_offset.unwrap();
        assert_eq!(&out[mdat_payload..mdat_payload + sample.len()], sample);
    }

    #[test]
    fn decrypt_uses_moof_start_plus_trun_data_offset_when_default_base_is_moof() {
        let key = [0x33u8; 16];
        let iv = [0x44u8; 8];
        let sample = b"HelloCENC world!"; // 16 bytes
        let file = build_cenc_file(key, iv, &[sample], true, false, false);
        let out = decrypt(file, &[], &key).unwrap();

        // Find mdat in the output and compare its payload to the plaintext.
        let mut pos = 0;
        let mut mdat_payload: Option<usize> = None;
        while pos < out.len() {
            let (t, b) = read_box(&out, pos).unwrap().unwrap();
            if &t == b"mdat" {
                mdat_payload = Some(b.payload);
                break;
            }
            pos = b.offset + b.size;
        }
        let mdat_payload = mdat_payload.expect("mdat box present");
        assert_eq!(&out[mdat_payload..mdat_payload + sample.len()], sample);
    }

    #[test]
    fn decrypt_errors_when_encrypted_traf_has_no_trun() {
        let key = [0x55u8; 16];
        let iv = [0x66u8; 8];
        let sample = b"abcdefgh";
        let file = build_cenc_file(key, iv, &[sample], true, true, true);
        match decrypt(file, &[], &key) {
            Err(DrmError::Decrypt(msg)) => {
                assert!(msg.to_lowercase().contains("trun"), "got: {msg}");
            }
            other => panic!("expected Decrypt(no trun), got {other:?}"),
        }
    }

    // ---- cbcs ----------------------------------------------------------------

    use aes::cipher::BlockCipherEncrypt;

    /// Encrypt `plaintext` with AES-128-CBC pattern encryption (cbcs).
    ///
    /// Mirror of `decrypt_cbcs_range`: we take the plaintext for each
    /// protected block run and produce ciphertext, leaving skipped blocks and
    /// trailing `len % 16` bytes untouched. Used by the unit tests to produce
    /// an encrypted fixture that `decrypt_samples_cbcs` can round-trip.
    fn encrypt_cbcs_range(
        key: &[u8; 16],
        iv0: &[u8; 16],
        buf: &mut [u8],
        crypt_byte_block: u8,
        skip_byte_block: u8,
    ) {
        let cipher = Aes128::new(key.into());
        let (cb, sb) = if crypt_byte_block == 0 && skip_byte_block == 0 {
            (u32::MAX, 0u32)
        } else {
            (u32::from(crypt_byte_block), u32::from(skip_byte_block))
        };
        let block_count = buf.len() / 16;
        let mut iv = *iv0;
        let mut block_idx = 0usize;
        while block_idx < block_count {
            let remaining = u32::try_from(block_count - block_idx).unwrap_or(u32::MAX);
            let crypt_now = cb.min(remaining);
            for _ in 0..crypt_now {
                let start = block_idx * 16;
                let end = start + 16;
                // CBC encrypt: xor plaintext with iv, then encrypt-block.
                for j in 0..16 {
                    buf[start + j] ^= iv[j];
                }
                let mut ct_slice = [0u8; 16];
                ct_slice.copy_from_slice(&buf[start..end]);
                let mut block: Block = ct_slice.into();
                cipher.encrypt_block(&mut block);
                buf[start..end].copy_from_slice(&block);
                iv.copy_from_slice(&buf[start..end]);
                block_idx += 1;
            }
            if sb == 0 {
                break;
            }
            let remaining_after_crypt = u32::try_from(block_count - block_idx).unwrap_or(u32::MAX);
            let skip_now = sb.min(remaining_after_crypt);
            block_idx += skip_now as usize;
        }
    }

    /// Standalone round-trip of `decrypt_samples_cbcs` with pattern 0:0:
    /// every complete 16-byte block of the protected range must be encrypted
    /// (no skipping). Encrypt a known plaintext with CBC, then decrypt via the
    /// production path and compare.
    #[test]
    fn cbcs_round_trip_pattern_0_0_decrypts_every_block() {
        let key = [0xA5u8; 16];
        let iv = [0x5Au8; 16];
        // 32 bytes = 2 complete AES blocks; no trailing clear tail.
        let plaintext: Vec<u8> = (0..32u8).collect();

        let mut buf = plaintext.clone();
        encrypt_cbcs_range(&key, &iv, &mut buf, 0, 0);
        assert_ne!(buf, plaintext, "encrypt should have changed the bytes");

        let senc = vec![SampleEncEntry {
            iv: Vec::new(),
            subsamples: Vec::new(),
        }];
        let params = CbcsParams {
            key: &key,
            constant_iv: &iv,
            crypt_byte_block: 0,
            skip_byte_block: 0,
        };
        decrypt_samples_cbcs(
            &mut buf,
            &[SampleSpan { start: 0, end: 32 }],
            &senc,
            &params,
        )
        .unwrap();
        assert_eq!(buf, plaintext);
    }

    /// Pattern 1:9 — video's pattern. In a 10-block protected range only
    /// block 0 should have been encrypted by the fixture; blocks 1..10 must
    /// remain in the clear (bytes 16..160 unchanged from plaintext).
    #[test]
    fn cbcs_encrypt_pattern_1_9_leaves_skipped_blocks_clear() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let plaintext: Vec<u8> = (0..160u32).map(|i| (i & 0xFF) as u8).collect();

        let mut buf = plaintext.clone();
        encrypt_cbcs_range(&key, &iv, &mut buf, 1, 9);

        assert_eq!(
            &buf[16..160],
            &plaintext[16..160],
            "skipped blocks must stay in the clear in a cbcs 1:9 ciphertext"
        );
        assert_ne!(
            &buf[..16],
            &plaintext[..16],
            "the one crypt block must have changed"
        );
    }

    /// End-to-end test: build a minimal MP4 with cbcs scheme (tenc v1,
    /// constant IV, pattern 1:9), then run `decrypt` and assert the mdat
    /// payload matches the plaintext.
    #[expect(
        clippy::similar_names,
        reason = "schi, schm, sinf, moov, moof are the spec's own four-letter box codes"
    )]
    #[test]
    fn decrypt_cbcs_tenc_v1_round_trips_one_sample() {
        let key = [0x33u8; 16];
        let constant_iv = [0x77u8; 16];
        let pattern_crypt: u8 = 1;
        let pattern_skip: u8 = 9;
        // 10 complete blocks = 160 bytes.
        let plaintext: Vec<u8> = (0..160u32).map(|i| ((i * 7) & 0xFF) as u8).collect();

        // --- build stsd with cbcs scheme and tenc v1 ------------------------
        // tenc v1 body:
        //   u8 reserved
        //   u8 ((crypt<<4) | skip)
        //   u8 default_isProtected
        //   u8 default_Per_Sample_IV_Size
        //   u8[16] default_KID
        //   u8 default_constant_IV_size
        //   u8[16] default_constant_IV
        let mut tenc_body: Vec<u8> = vec![
            0,
            (pattern_crypt << 4) | pattern_skip,
            1,
            0, // per-sample IV size = 0; constant IV follows
        ];
        tenc_body.extend_from_slice(&[0xAAu8; 16]); // KID
        tenc_body.push(16);
        tenc_body.extend_from_slice(&constant_iv);
        let tenc = mk_full_box(*b"tenc", 1, 0, &tenc_body);
        let schi = mk_box(*b"schi", &tenc);
        let schm_payload = concat_bytes(&[b"cbcs", &0x0001_0000u32.to_be_bytes()]);
        let schm = mk_full_box(*b"schm", 0, 0, &schm_payload);
        let frma = mk_box(*b"frma", b"mp4a");
        let sinf_children = concat_bytes(&[&frma, &schm, &schi]);
        let sinf = mk_box(*b"sinf", &sinf_children);
        let prelude = [0u8; AUDIO_SAMPLE_ENTRY_PRELUDE];
        let enca_payload = concat_bytes(&[&prelude, &sinf]);
        let enca = mk_box(*b"enca", &enca_payload);
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&1u32.to_be_bytes());
        stsd_body.extend_from_slice(&enca);
        let stsd = mk_full_box(*b"stsd", 0, 0, &stsd_body);
        let moov = mk_moov_with_stsd(&stsd);
        let ftyp = mk_box(*b"ftyp", b"iso5\0\0\0\0mp42");

        // --- encrypt the sample into mdat with pattern 1:9 ------------------
        let mut mdat_payload = plaintext.clone();
        encrypt_cbcs_range(
            &key,
            &constant_iv,
            &mut mdat_payload,
            pattern_crypt,
            pattern_skip,
        );
        let mdat = mk_box(*b"mdat", &mdat_payload);

        // --- build moof (tfhd with default-base-is-moof + default_sample_size;
        //     trun with data_offset; senc with no per-sample IVs) -------------
        let mut tfhd_body = Vec::new();
        tfhd_body.extend_from_slice(&1u32.to_be_bytes());
        tfhd_body.extend_from_slice(&u32::try_from(plaintext.len()).unwrap().to_be_bytes());
        let tfhd_flags = 0x0002_0010u32;
        let tfhd = mk_full_box(*b"tfhd", 0, tfhd_flags, &tfhd_body);
        // senc: sample_count=1, no IV bytes (default_Per_Sample_IV_Size = 0),
        // no subsamples.
        let mut senc_body = Vec::new();
        senc_body.extend_from_slice(&1u32.to_be_bytes());
        let senc_box = mk_full_box(*b"senc", 0, 0, &senc_body);
        let mfhd_body = 1u32.to_be_bytes();
        let mfhd = mk_full_box(*b"mfhd", 0, 0, &mfhd_body);

        let moof_builder = |data_offset: i32| -> Vec<u8> {
            let mut trun_body = Vec::new();
            trun_body.extend_from_slice(&1u32.to_be_bytes());
            trun_body.extend_from_slice(&data_offset.to_be_bytes());
            let trun = mk_full_box(*b"trun", 0, 0x0000_0001, &trun_body);
            let mut traf_children = Vec::new();
            traf_children.extend_from_slice(&tfhd);
            traf_children.extend_from_slice(&trun);
            traf_children.extend_from_slice(&senc_box);
            let traf = mk_box(*b"traf", &traf_children);
            let mut moof_children = Vec::new();
            moof_children.extend_from_slice(&mfhd);
            moof_children.extend_from_slice(&traf);
            mk_box(*b"moof", &moof_children)
        };
        let moof_size = moof_builder(0).len();
        let moov_end_in_file = ftyp.len() + moov.len();
        let moof_start_in_file = moov_end_in_file;
        let mdat_payload_start_in_file = moof_start_in_file + moof_size + 8;
        let data_offset = i32::try_from(mdat_payload_start_in_file - moof_start_in_file).unwrap();
        let moof = moof_builder(data_offset);
        let file = concat_bytes(&[&ftyp, &moov, &moof, &mdat]);

        // --- act: decrypt -----------------------------------------------------
        let out = decrypt(file, &[], &key).unwrap();

        // --- assert: mdat payload == plaintext --------------------------------
        let mut pos = 0;
        let mut recovered = None;
        while pos < out.len() {
            let (t, b) = read_box(&out, pos).unwrap().unwrap();
            if &t == b"mdat" {
                recovered = Some(b.payload);
                break;
            }
            pos = b.offset + b.size;
        }
        let mdat_payload_off = recovered.expect("mdat present in output");
        assert_eq!(
            &out[mdat_payload_off..mdat_payload_off + plaintext.len()],
            &plaintext[..],
            "cbcs round-trip must recover the exact plaintext"
        );
    }

    /// `decrypt_samples_cbcs` with pattern 1:9 recovers only the encrypted
    /// blocks; the nine skipped blocks remain untouched (they were plaintext
    /// in the clear in the ciphertext, so decrypt must not change them).
    #[test]
    fn cbcs_pattern_1_9_direct_call_recovers_plaintext() {
        let key = [0x44u8; 16];
        let iv = [0x88u8; 16];
        // 10 blocks total; block 0 crypt, blocks 1..10 skipped.
        let plaintext: Vec<u8> = (0..160u8).collect();

        let mut buf = plaintext.clone();
        encrypt_cbcs_range(&key, &iv, &mut buf, 1, 9);

        let senc = vec![SampleEncEntry {
            iv: Vec::new(),
            subsamples: Vec::new(),
        }];
        let params = CbcsParams {
            key: &key,
            constant_iv: &iv,
            crypt_byte_block: 1,
            skip_byte_block: 9,
        };
        decrypt_samples_cbcs(
            &mut buf,
            &[SampleSpan { start: 0, end: 160 }],
            &senc,
            &params,
        )
        .unwrap();
        assert_eq!(buf, plaintext);
    }

    // ---- cbcs known-answer + multi-run / multi-fragment pairing -----------

    fn hex16(s: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex");
        }
        out
    }

    /// Known-answer test from NIST SP 800-38A F.2.2 (CBC-AES128.Decrypt),
    /// laid out as a cbcs 1:1 pattern: block 0 = C1, block 1 skipped (clear),
    /// block 2 = C2, plus a 5-byte partial tail. CBC chains across the skipped
    /// block, so C2 must decrypt with C1 as its chaining value, and the
    /// skipped block and tail must come out byte-identical.
    #[test]
    fn cbcs_pattern_1_1_matches_nist_sp800_38a_vectors() {
        let key = hex16("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex16("000102030405060708090a0b0c0d0e0f");
        let c1 = hex16("7649abac8119b246cee98e9b12e9197d");
        let c2 = hex16("5086cb9b507219ee95db113a917678b2");
        let p1 = hex16("6bc1bee22e409f96e93d7e117393172a");
        let p2 = hex16("ae2d8a571e03ac9c9eb76fac45af8e51");
        let skipped = [0xC3u8; 16];
        let tail = [1u8, 2, 3, 4, 5];

        let mut range = concat_bytes(&[&c1, &skipped, &c2, &tail]);
        let cipher = Aes128::new((&key).into());
        let mut chain = iv;
        decrypt_cbcs_range(&cipher, &mut range, &mut chain, 1, 1);

        assert_eq!(range, concat_bytes(&[&p1, &skipped, &p2, &tail]));
    }

    /// Init segment (`ftyp` + `moov`) for a cbcs audio track: tenc v1 with a
    /// 16-byte constant IV and the given pattern, plus a `moov/pssh`.
    #[expect(
        clippy::similar_names,
        reason = "schi, schm and sinf are the spec's own four-letter box codes"
    )]
    fn mk_cbcs_init(constant_iv: &[u8; 16], crypt: u8, skip: u8) -> Vec<u8> {
        let mut tenc_body = vec![0, (crypt << 4) | skip, 1, 0];
        tenc_body.extend_from_slice(&[0xAAu8; 16]);
        tenc_body.push(16);
        tenc_body.extend_from_slice(constant_iv);
        let tenc = mk_full_box(*b"tenc", 1, 0, &tenc_body);
        let schi = mk_box(*b"schi", &tenc);
        let schm = mk_full_box(
            *b"schm",
            0,
            0,
            &concat_bytes(&[b"cbcs", &0x0001_0000u32.to_be_bytes()]),
        );
        let frma = mk_box(*b"frma", b"mp4a");
        let sinf = mk_box(*b"sinf", &concat_bytes(&[&frma, &schm, &schi]));
        let enca = mk_box(
            *b"enca",
            &concat_bytes(&[&[0u8; AUDIO_SAMPLE_ENTRY_PRELUDE], &sinf]),
        );
        let stsd = mk_full_box(*b"stsd", 0, 0, &concat_bytes(&[&1u32.to_be_bytes(), &enca]));
        let stbl = mk_box(*b"stbl", &stsd);
        let minf = mk_box(*b"minf", &stbl);
        let mdia = mk_box(*b"mdia", &minf);
        let trak = mk_box(*b"trak", &mdia);
        let pssh = mk_full_box(*b"pssh", 0, 0, &[0u8; 20]);
        let moov = mk_box(*b"moov", &concat_bytes(&[&pssh, &trak]));
        let ftyp = mk_box(*b"ftyp", b"iso5\0\0\0\0mp42");
        concat_bytes(&[&ftyp, &moov])
    }

    fn u32_len(n: usize) -> [u8; 4] {
        u32::try_from(n).expect("fits in u32").to_be_bytes()
    }

    /// One `moof` + `mdat` whose single `traf` holds one `trun` per entry of
    /// `runs` (each a list of already-encrypted samples). Every run carries
    /// per-sample sizes; only the first run carries `data_offset` unless
    /// `offset_on_every_run`, so later runs exercise the "continue after the
    /// previous run" rule. `senc` is written verbatim as the traf's last child.
    fn mk_fragment(runs: &[Vec<Vec<u8>>], offset_on_every_run: bool, senc: &[u8]) -> Vec<u8> {
        mk_fragment_with_tfdt(runs, offset_on_every_run, senc, &[])
    }

    /// [`mk_fragment`] with `tfdt` (a whole box, or empty for none) written
    /// right after the traf's `tfhd`.
    fn mk_fragment_with_tfdt(
        runs: &[Vec<Vec<u8>>],
        offset_on_every_run: bool,
        senc: &[u8],
        tfdt: &[u8],
    ) -> Vec<u8> {
        // tfhd: track 1, default-base-is-moof.
        let tfhd = mk_full_box(*b"tfhd", 0, 0x0002_0000, &1u32.to_be_bytes());
        let mfhd = mk_full_box(*b"mfhd", 0, 0, &1u32.to_be_bytes());

        let build = |first_data_offset: usize| -> Vec<u8> {
            let mut traf_children = concat_bytes(&[&tfhd, tfdt]);
            let mut run_offset = first_data_offset;
            for (k, run) in runs.iter().enumerate() {
                let with_offset = k == 0 || offset_on_every_run;
                let flags = if with_offset { 0x0201 } else { 0x0200 };
                let mut body = u32_len(run.len()).to_vec();
                if with_offset {
                    body.extend_from_slice(&i32::try_from(run_offset).expect("fits").to_be_bytes());
                }
                for sample in run {
                    body.extend_from_slice(&u32_len(sample.len()));
                }
                traf_children.extend_from_slice(&mk_full_box(*b"trun", 0, flags, &body));
                run_offset += run.iter().map(Vec::len).sum::<usize>();
            }
            traf_children.extend_from_slice(senc);
            let traf = mk_box(*b"traf", &traf_children);
            mk_box(*b"moof", &concat_bytes(&[&mfhd, &traf]))
        };
        // The moof size does not depend on the offset values.
        let moof_size = build(0).len();
        let moof = build(moof_size + 8);
        let payload: Vec<u8> = runs.iter().flatten().flatten().copied().collect();
        concat_bytes(&[&moof, &mk_box(*b"mdat", &payload)])
    }

    /// A `senc` with `count` entries and no per-sample IVs (cbcs constant IV).
    fn mk_ivless_senc(count: usize) -> Vec<u8> {
        mk_full_box(*b"senc", 0, 0, &u32_len(count))
    }

    /// Plaintext sample `n` of length `len` (deterministic, distinct per n).
    fn sample_plaintext(n: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((i * 31 + n * 7) & 0xFF).expect("masked"))
            .collect()
    }

    /// Encrypts each plaintext sample with cbcs (constant IV reset per sample).
    fn cbcs_encrypt_runs(
        key: &[u8; 16],
        iv: &[u8; 16],
        crypt: u8,
        skip: u8,
        runs: &[Vec<Vec<u8>>],
    ) -> Vec<Vec<Vec<u8>>> {
        runs.iter()
            .map(|run| {
                run.iter()
                    .map(|pt| {
                        let mut ct = pt.clone();
                        encrypt_cbcs_range(key, iv, &mut ct, crypt, skip);
                        ct
                    })
                    .collect()
            })
            .collect()
    }

    /// Payloads of every `mdat` in `file`, concatenated.
    fn all_mdat_payloads(file: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((t, b)) = read_box(file, pos).unwrap() {
            if &t == b"mdat" {
                out.extend_from_slice(&file[b.payload..b.offset + b.size]);
            }
            pos = b.offset + b.size;
        }
        out
    }

    /// Mirrors Apple's music-video audio: several `trun`s per `traf` and
    /// several `moof`s per file, with one `senc` covering all of a `traf`'s
    /// runs. Sizes include partial trailing blocks (left clear by cbcs).
    #[test]
    fn decrypt_cbcs_pairs_senc_with_all_truns_across_fragments() {
        let key = [0x5Cu8; 16];
        let iv = [0x3Du8; 16];
        let (crypt, skip) = (1, 9);
        let fragments: Vec<Vec<Vec<Vec<u8>>>> = (0..2)
            .map(|f| {
                (0..3)
                    .map(|r| {
                        (0..3)
                            .map(|s| sample_plaintext(f * 100 + r * 10 + s, 37 + 64 * s + 16 * r))
                            .collect()
                    })
                    .collect()
            })
            .collect();

        let mut file = mk_cbcs_init(&iv, crypt, skip);
        for (f, runs) in fragments.iter().enumerate() {
            let encrypted = cbcs_encrypt_runs(&key, &iv, crypt, skip, runs);
            // Fragment 0: data_offset on the first run only; fragment 1: on all.
            file.extend_from_slice(&mk_fragment(&encrypted, f == 1, &mk_ivless_senc(9)));
        }

        let out = decrypt(file, &[], &key).unwrap();

        let expected: Vec<u8> = fragments
            .iter()
            .flatten()
            .flatten()
            .flatten()
            .copied()
            .collect();
        assert_eq!(all_mdat_payloads(&out), expected);
    }

    #[test]
    fn decrypt_cenc_pairs_senc_with_multiple_truns() {
        use ctr::cipher::{KeyIvInit, StreamCipher};

        let key = [0x21u8; 16];
        let ivs: Vec<[u8; 8]> = (1..=4u8).map(|i| [i; 8]).collect();
        let plaintexts: Vec<Vec<u8>> = (0..4).map(|n| sample_plaintext(n, 20 + n)).collect();
        let encrypted: Vec<Vec<u8>> = plaintexts
            .iter()
            .zip(&ivs)
            .map(|(pt, iv)| {
                let mut block = [0u8; 16];
                block[..8].copy_from_slice(iv);
                let mut ct = pt.clone();
                Aes128Ctr::new((&key).into(), (&block).into()).apply_keystream(&mut ct);
                ct
            })
            .collect();
        let runs = vec![encrypted[..2].to_vec(), encrypted[2..].to_vec()];
        let mut senc_body = u32_len(4).to_vec();
        for iv in &ivs {
            senc_body.extend_from_slice(iv);
        }
        let senc = mk_full_box(*b"senc", 0, 0, &senc_body);

        let mut file = mk_box(*b"ftyp", b"iso5\0\0\0\0mp42");
        file.extend_from_slice(&mk_moov_with_stsd(&mk_stsd(*b"cenc", 8)));
        file.extend_from_slice(&mk_fragment(&runs, false, &senc));

        let out = decrypt(file, &[], &key).unwrap();

        let expected: Vec<u8> = plaintexts.iter().flatten().copied().collect();
        assert_eq!(all_mdat_payloads(&out), expected);
    }

    #[test]
    fn decrypt_errors_when_senc_count_differs_from_all_truns() {
        let key = [0x5Cu8; 16];
        let iv = [0x3Du8; 16];
        let runs = vec![
            vec![sample_plaintext(0, 32); 2],
            vec![sample_plaintext(1, 32); 2],
        ];
        let mut file = mk_cbcs_init(&iv, 1, 9);
        file.extend_from_slice(&mk_fragment(&runs, false, &mk_ivless_senc(5)));

        match decrypt(file, &[], &key) {
            Err(DrmError::Decrypt(msg)) => assert!(msg.contains("5 senc vs 4 trun"), "got: {msg}"),
            other => panic!("expected a count mismatch, got {other:?}"),
        }
    }

    #[test]
    fn decrypt_cbcs_output_has_no_protection_boxes_left() {
        let key = [0x5Cu8; 16];
        let iv = [0x3Du8; 16];
        let runs = vec![vec![sample_plaintext(0, 48)], vec![sample_plaintext(1, 21)]];
        let encrypted = cbcs_encrypt_runs(&key, &iv, 1, 9, &runs);
        let mut file = mk_cbcs_init(&iv, 1, 9);
        file.extend_from_slice(&mk_fragment(&encrypted, false, &mk_ivless_senc(2)));

        let out = decrypt(file, &[], &key).unwrap();

        for marker in [b"enca", b"sinf", b"senc", b"pssh"] {
            assert!(
                find_subslice(&out, marker).is_none(),
                "{} still present",
                String::from_utf8_lossy(marker)
            );
        }
    }

    // --- tfdt rebase ---------------------------------------------------------

    /// First `tfdt` of Apple's music-video audio: 9.956 s at 48 kHz.
    const MV_AUDIO_BASE: u64 = 477_888;
    /// One AAC frame at 48 kHz.
    const FRAME: u64 = 1024;

    /// A `tfdt` box of the given version holding `time`.
    fn mk_tfdt(version: u8, time: u64) -> Vec<u8> {
        let body = if version == 1 {
            time.to_be_bytes().to_vec()
        } else {
            u32::try_from(time)
                .expect("fits in u32")
                .to_be_bytes()
                .to_vec()
        };
        mk_full_box(*b"tfdt", version, 0, &body)
    }

    /// Init segment plus one single-sample fragment per entry of `tfdts`.
    fn mk_timed_file(tfdts: &[Vec<u8>]) -> Vec<u8> {
        let mut file = mk_cbcs_init(&[0x3D; 16], 1, 9);
        for tfdt in tfdts {
            let runs = vec![vec![sample_plaintext(0, 32)]];
            file.extend_from_slice(&mk_fragment_with_tfdt(
                &runs,
                false,
                &mk_ivless_senc(1),
                tfdt,
            ));
        }
        file
    }

    /// Every `moof/traf/tfdt` value in file order.
    fn read_tfdts(file: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((t, moof)) = read_box(file, pos).unwrap() {
            pos = moof.offset + moof.size;
            if &t != b"moof" {
                continue;
            }
            let mut p = moof.payload;
            while p < moof.offset + moof.size {
                let (t, traf) = read_box(file, p).unwrap().unwrap();
                p = traf.offset + traf.size;
                if &t == b"traf" {
                    let tfdt = find_box(file, traf.payload, p, *b"tfdt").unwrap().unwrap();
                    let (version, _, body) = full_box_header(file, tfdt).unwrap();
                    let width = if version == 1 { 8 } else { 4 };
                    let time = file[body..body + width]
                        .iter()
                        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                    out.push(time);
                }
            }
        }
        out
    }

    #[test]
    fn rebase_shifts_v0_tfdts_to_start_at_zero() {
        let mut file = mk_timed_file(&[
            mk_tfdt(0, MV_AUDIO_BASE),
            mk_tfdt(0, MV_AUDIO_BASE + 23 * FRAME),
        ]);

        rebase_fragment_times(&mut file).unwrap();

        assert_eq!(read_tfdts(&file), [0, 23 * FRAME]);
    }

    #[test]
    fn rebase_shifts_v1_tfdts_wider_than_u32() {
        let base = (1u64 << 32) + MV_AUDIO_BASE;
        let mut file = mk_timed_file(&[mk_tfdt(1, base), mk_tfdt(1, base + 7 * FRAME)]);
        let len = file.len();

        rebase_fragment_times(&mut file).unwrap();

        assert_eq!((read_tfdts(&file), file.len()), (vec![0, 7 * FRAME], len));
    }

    #[test]
    fn rebase_leaves_file_starting_at_zero_unchanged() {
        let original = mk_timed_file(&[mk_tfdt(0, 0), mk_tfdt(1, 5 * FRAME)]);
        let mut file = original.clone();

        rebase_fragment_times(&mut file).unwrap();

        assert_eq!(file, original);
    }

    #[test]
    fn rebase_errors_when_a_later_tfdt_is_before_the_first() {
        let mut file = mk_timed_file(&[mk_tfdt(0, MV_AUDIO_BASE), mk_tfdt(0, FRAME)]);

        match rebase_fragment_times(&mut file) {
            Err(DrmError::Decrypt(msg)) => assert!(msg.contains("before its first"), "got: {msg}"),
            other => panic!("expected an underflow error, got {other:?}"),
        }
    }

    #[test]
    fn rebase_uses_a_separate_base_per_track() {
        let traf = |track: u32, time: u64| {
            let tfhd = mk_full_box(*b"tfhd", 0, 0, &track.to_be_bytes());
            mk_box(*b"traf", &concat_bytes(&[&tfhd, &mk_tfdt(0, time)]))
        };
        let moof = |a: u64, b: u64| mk_box(*b"moof", &concat_bytes(&[&traf(1, a), &traf(2, b)]));
        let mut file = concat_bytes(&[&moof(1000, 50), &moof(3048, 2098)]);

        rebase_fragment_times(&mut file).unwrap();

        assert_eq!(read_tfdts(&file), [0, 0, 2048, 2048]);
    }

    #[test]
    fn decrypt_rebases_music_video_audio_timeline() {
        let key = [0x5Cu8; 16];
        let iv = [0x3Du8; 16];
        let encrypted = cbcs_encrypt_runs(&key, &iv, 1, 9, &[vec![sample_plaintext(0, 32)]]);
        let mut file = mk_cbcs_init(&iv, 1, 9);
        for time in [MV_AUDIO_BASE, MV_AUDIO_BASE + FRAME] {
            file.extend_from_slice(&mk_fragment_with_tfdt(
                &encrypted,
                false,
                &mk_ivless_senc(1),
                &mk_tfdt(0, time),
            ));
        }

        let out = decrypt(file, &[], &key).unwrap();

        assert_eq!(read_tfdts(&out), [0, FRAME]);
    }
}
