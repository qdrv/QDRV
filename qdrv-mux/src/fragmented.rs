// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Fragmented ISOBMFF (fMP4) and CMAF muxing for QDRV delivery streams.
//!
//! Where [`crate::write_mp4`] emits a single-`mdat` *progressive* file — valid
//! for download-and-play but not segmentable — this module emits **fragmented**
//! ISOBMFF: an initialisation segment (`ftyp` + `moov` with a `mvex`/`trex`
//! that declares fragments and empty sample tables) followed by one media
//! segment (`styp` + `moof` + `mdat`) per group of pictures, split at AV1
//! keyframes. That is the structure MPEG-DASH and low-latency HLS consume, so
//! it is what lets a QDRV delivery stream participate in modern adaptive
//! streaming instead of only file playback.
//!
//! [`write_cmaf`] is the same writer with CMAF brands (ISO/IEC 23000-19) so a
//! single set of segments is accepted by both DASH and HLS packagers;
//! [`write_fmp4`] uses generic fragmented-ISOBMFF brands.
//!
//! ## Carriage of HDR and dynamic metadata
//!
//! The AV1 sequence header inside the bitstream carries the Rec. 2020 primaries
//! and SMPTE ST 2084 transfer signalling (set by the encoder), and the sample
//! entry repeats it in a `colr nclx` box. QDRV's per-frame dynamic metadata is
//! carried **inside the AV1 elementary stream** as ITU-T T.35 metadata OBUs
//! (see `qdrv_codec::embed_qdrv_metadata`), embedded by the QDRV muxer before
//! these segments are written — so it travels with the bitstream into every
//! container target uniformly, instead of via a per-container sidecar.

use std::io::Write;

use crate::{
    DEFAULT_MP4_TIMESCALE, Mp4Config, MuxError, MuxFrame, box_size_u32, build_dinf, build_hdlr,
    build_mdhd, build_mvhd, build_stsd, build_tkhd, build_vmhd, compute_frame_duration,
    usize_to_u32,
};

/// AV1 sync-sample (keyframe) flags: `sample_depends_on = 2` (depends on
/// nothing), `sample_is_non_sync_sample = 0`. Per ISO/IEC 14496-12 §8.8.3.1.
const SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;
/// Non-sync (inter) sample flags: `sample_depends_on = 1`,
/// `sample_is_non_sync_sample = 1`.
const SAMPLE_FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// `trun` flags: data-offset-present (`0x1`) + first-sample-flags-present
/// (`0x4`) + sample-size-present (`0x200`). Every sample carries its own size;
/// duration comes from `trex`; the first sample of each segment overrides the
/// default flags to mark it a sync sample.
const TRUN_FLAGS: u32 = 0x0000_0205;

/// `tfhd` flags: default-base-is-moof (`0x020000`), so sample data offsets are
/// resolved relative to the enclosing `moof` — the CMAF-required addressing
/// mode. No other `tfhd` optional fields are present; durations, sizes, and
/// default flags all come from `trex`.
const TFHD_FLAGS_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;

/// Largest `mdat` payload that still fits the classic 32-bit box size; above
/// this a 64-bit `largesize` header is emitted. A single GOP fragment never
/// approaches this, but the writer stays correct if one ever did.
const MAX_FRAGMENT_MDAT_U32_PAYLOAD: u64 = u32::MAX as u64 - 8;

/// Brand profile selecting the `ftyp`/`styp` brands written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrandProfile {
    /// Generic fragmented ISOBMFF.
    Fmp4,
    /// CMAF (ISO/IEC 23000-19) track + media-segment brands.
    Cmaf,
}

/// Writes the AV1 frames as a generic fragmented ISOBMFF (`.mp4`) stream.
///
/// Produces an initialisation segment followed by one keyframe-aligned media
/// segment per GOP. The result is a valid fragmented MP4 playable by any
/// AV1-capable ISOBMFF player and ready to be split into DASH segments.
///
/// # Errors
/// See [`write_cmaf`]; the two share all validation and size-overflow paths.
pub fn write_fmp4<W: Write>(
    writer: &mut W,
    config: &Mp4Config,
    frames: &[MuxFrame],
) -> Result<(), MuxError> {
    write_fragmented(writer, config, frames, BrandProfile::Fmp4)
}

/// Writes the AV1 frames as a CMAF (ISO/IEC 23000-19) fragmented stream.
///
/// Identical structure to [`write_fmp4`] but with CMAF brands, so one set of
/// segments is accepted by both MPEG-DASH and HLS packagers without repackaging.
///
/// # Errors
/// - [`MuxError::NoFrames`] — `frames` is empty.
/// - [`MuxError::Io`] — the writer failed, or `frame_rate`/dimensions are out
///   of range (dimensions must fit the 16-bit ISOBMFF sample-entry fields).
/// - [`MuxError::SizeOverflow`] — a box size, sample size, sample count, or
///   decode-time computation does not fit its field.
pub fn write_cmaf<W: Write>(
    writer: &mut W,
    config: &Mp4Config,
    frames: &[MuxFrame],
) -> Result<(), MuxError> {
    write_fragmented(writer, config, frames, BrandProfile::Cmaf)
}

fn write_fragmented<W: Write>(
    writer: &mut W,
    config: &Mp4Config,
    frames: &[MuxFrame],
    profile: BrandProfile,
) -> Result<(), MuxError> {
    if frames.is_empty() {
        return Err(MuxError::NoFrames);
    }
    if !config.frame_rate.is_finite() || config.frame_rate <= 0.0 {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "frame_rate must be positive and finite, got {}",
                config.frame_rate
            ),
        )));
    }
    if config.width == 0
        || config.height == 0
        || config.width > u16::MAX as u32
        || config.height > u16::MAX as u32
    {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "fragmented MP4 sample entry dimensions must be in 1..={} (got {}x{})",
                u16::MAX,
                config.width,
                config.height
            ),
        )));
    }

    let timescale = DEFAULT_MP4_TIMESCALE;
    let frame_duration = compute_frame_duration(timescale, config.frame_rate)?;
    let frame_count = u64::try_from(frames.len()).map_err(|_| MuxError::SizeOverflow {
        context: "fragmented frame count",
    })?;
    let total_duration =
        u64::from(frame_duration)
            .checked_mul(frame_count)
            .ok_or(MuxError::SizeOverflow {
                context: "fragmented total duration",
            })?;

    // Initialisation segment: ftyp + moov (with mvex declaring fragments).
    let ftyp = build_init_ftyp(profile)?;
    let moov = build_init_moov(config, timescale, total_duration, frame_duration)?;
    writer.write_all(&ftyp)?;
    writer.write_all(&moov)?;

    // Media segments, one per GOP (a run starting at a keyframe).
    let styp = build_styp(profile)?;
    let mut sequence_number: u32 = 1;
    for (start, end) in keyframe_segments(frames) {
        let base_decode_time = (start as u64)
            .checked_mul(u64::from(frame_duration))
            .ok_or(MuxError::SizeOverflow {
                context: "fragment base media decode time",
            })?;
        let (moof, mdat) = build_segment(sequence_number, base_decode_time, &frames[start..end])?;
        writer.write_all(&styp)?;
        writer.write_all(&moof)?;
        writer.write_all(&mdat)?;
        sequence_number = sequence_number
            .checked_add(1)
            .ok_or(MuxError::SizeOverflow {
                context: "fragment sequence number",
            })?;
    }

    Ok(())
}

/// Returns `[start, end)` frame ranges, one per media segment. A new segment
/// begins at index 0 and at every keyframe, so each segment starts with a sync
/// sample (the CMAF stream-access-point requirement).
fn keyframe_segments(frames: &[MuxFrame]) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    for (i, frame) in frames.iter().enumerate().skip(1) {
        if frame.is_keyframe {
            segments.push((start, i));
            start = i;
        }
    }
    segments.push((start, frames.len()));
    segments
}

// ---------------------------------------------------------------------------
// Box assembly helpers
// ---------------------------------------------------------------------------

/// Wraps `payload` in an ISOBMFF box (`size` + `type` + payload) with checked
/// 32-bit size arithmetic.
fn mp4_box(box_type: &[u8; 4], payload: &[u8], context: &'static str) -> Result<Vec<u8>, MuxError> {
    let size = box_size_u32(payload.len(), context)?;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Builds an `ftyp`/`styp`-style brand box.
fn brand_box(
    box_type: &[u8; 4],
    major: &[u8; 4],
    minor: u32,
    compatible: &[&[u8; 4]],
    context: &'static str,
) -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::with_capacity(8 + compatible.len() * 4);
    payload.extend_from_slice(major);
    payload.extend_from_slice(&minor.to_be_bytes());
    for brand in compatible {
        payload.extend_from_slice(*brand);
    }
    mp4_box(box_type, &payload, context)
}

fn build_init_ftyp(profile: BrandProfile) -> Result<Vec<u8>, MuxError> {
    match profile {
        BrandProfile::Fmp4 => brand_box(
            b"ftyp",
            b"iso6",
            0,
            &[b"iso6", b"iso5", b"av01", b"mp41"],
            "ftyp box",
        ),
        BrandProfile::Cmaf => brand_box(
            b"ftyp",
            b"cmfc",
            0,
            &[b"cmfc", b"iso6", b"av01"],
            "ftyp box",
        ),
    }
}

fn build_styp(profile: BrandProfile) -> Result<Vec<u8>, MuxError> {
    match profile {
        BrandProfile::Fmp4 => brand_box(b"styp", b"msdh", 0, &[b"msdh", b"msix"], "styp box"),
        BrandProfile::Cmaf => brand_box(
            b"styp",
            b"msdh",
            0,
            &[b"msdh", b"msix", b"cmfs"],
            "styp box",
        ),
    }
}

fn build_init_moov(
    config: &Mp4Config,
    timescale: u32,
    total_duration: u64,
    frame_duration: u32,
) -> Result<Vec<u8>, MuxError> {
    let mvhd = build_mvhd(timescale, total_duration);
    let trak = build_init_trak(config, timescale, total_duration)?;
    let mvex = build_mvex(frame_duration)?;
    let mut payload = Vec::with_capacity(mvhd.len() + trak.len() + mvex.len());
    payload.extend_from_slice(&mvhd);
    payload.extend_from_slice(&trak);
    payload.extend_from_slice(&mvex);
    mp4_box(b"moov", &payload, "moov box")
}

fn build_init_trak(config: &Mp4Config, timescale: u32, duration: u64) -> Result<Vec<u8>, MuxError> {
    let tkhd = build_tkhd(config, duration);
    let mdia = build_init_mdia(config, timescale, duration)?;
    let mut payload = Vec::with_capacity(tkhd.len() + mdia.len());
    payload.extend_from_slice(&tkhd);
    payload.extend_from_slice(&mdia);
    mp4_box(b"trak", &payload, "trak box")
}

fn build_init_mdia(config: &Mp4Config, timescale: u32, duration: u64) -> Result<Vec<u8>, MuxError> {
    let mdhd = build_mdhd(timescale, duration);
    let hdlr = build_hdlr();
    let minf = build_init_minf(config)?;
    let mut payload = Vec::with_capacity(mdhd.len() + hdlr.len() + minf.len());
    payload.extend_from_slice(&mdhd);
    payload.extend_from_slice(&hdlr);
    payload.extend_from_slice(&minf);
    mp4_box(b"mdia", &payload, "mdia box")
}

fn build_init_minf(config: &Mp4Config) -> Result<Vec<u8>, MuxError> {
    let vmhd = build_vmhd();
    let dinf = build_dinf();
    let stbl = build_empty_stbl(config)?;
    let mut payload = Vec::with_capacity(vmhd.len() + dinf.len() + stbl.len());
    payload.extend_from_slice(&vmhd);
    payload.extend_from_slice(&dinf);
    payload.extend_from_slice(&stbl);
    mp4_box(b"minf", &payload, "minf box")
}

/// Sample table for a fragmented track: the `stsd` sample description is real
/// (av01 + av1C + colr), but `stts`/`stsc`/`stsz`/`stco` are all zero-entry
/// because the samples live in the media-segment fragments, not the `moov`.
fn build_empty_stbl(config: &Mp4Config) -> Result<Vec<u8>, MuxError> {
    let stsd = build_stsd(config)?;
    // stts/stsc/stco: FullBox header (4) + entry_count = 0 (4).
    let empty_count = [0u8; 8];
    // stsz: FullBox header (4) + sample_size = 0 (4) + sample_count = 0 (4).
    let empty_stsz = [0u8; 12];
    let stts = mp4_box(b"stts", &empty_count, "stts box")?;
    let stsc = mp4_box(b"stsc", &empty_count, "stsc box")?;
    let stsz = mp4_box(b"stsz", &empty_stsz, "stsz box")?;
    let stco = mp4_box(b"stco", &empty_count, "stco box")?;

    let mut payload =
        Vec::with_capacity(stsd.len() + stts.len() + stsc.len() + stsz.len() + stco.len());
    payload.extend_from_slice(&stsd);
    payload.extend_from_slice(&stts);
    payload.extend_from_slice(&stsc);
    payload.extend_from_slice(&stsz);
    payload.extend_from_slice(&stco);
    mp4_box(b"stbl", &payload, "stbl box")
}

fn build_mvex(frame_duration: u32) -> Result<Vec<u8>, MuxError> {
    // trex: version+flags(4) + track_ID(4) + default_sample_description_index(4)
    //       + default_sample_duration(4) + default_sample_size(4)
    //       + default_sample_flags(4).
    let mut trex_payload = Vec::with_capacity(24);
    trex_payload.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    trex_payload.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    trex_payload.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    trex_payload.extend_from_slice(&frame_duration.to_be_bytes());
    trex_payload.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size (per-sample in trun)
    trex_payload.extend_from_slice(&SAMPLE_FLAGS_NON_SYNC.to_be_bytes());
    let trex = mp4_box(b"trex", &trex_payload, "trex box")?;
    mp4_box(b"mvex", &trex, "mvex box")
}

/// Builds one media segment's `moof` and `mdat` for the given frame run. The
/// `trun` `data_offset` is patched after the `moof` is assembled, so it points
/// at the first sample byte regardless of how the box sizes fall out.
fn build_segment(
    sequence_number: u32,
    base_decode_time: u64,
    frames: &[MuxFrame],
) -> Result<(Vec<u8>, Vec<u8>), MuxError> {
    debug_assert!(
        !frames.is_empty(),
        "segment must contain at least one frame"
    );
    let sample_count = usize_to_u32(frames.len(), "fragment sample count")?;

    let mut sizes: Vec<u32> = Vec::with_capacity(frames.len());
    let mut mdat_payload: Vec<u8> = Vec::new();
    for frame in frames {
        sizes.push(usize_to_u32(frame.av1_data.len(), "fragment sample size")?);
        mdat_payload
            .try_reserve(frame.av1_data.len())
            .map_err(|_| MuxError::SizeOverflow {
                context: "fragment mdat payload allocation",
            })?;
        mdat_payload.extend_from_slice(&frame.av1_data);
    }
    let first_is_sync = frames[0].is_keyframe;

    // mfhd: version+flags(4) + sequence_number(4).
    let mut mfhd_payload = Vec::with_capacity(8);
    mfhd_payload.extend_from_slice(&0u32.to_be_bytes());
    mfhd_payload.extend_from_slice(&sequence_number.to_be_bytes());
    let mfhd = mp4_box(b"mfhd", &mfhd_payload, "mfhd box")?;

    // tfhd: version+flags(4) + track_ID(4). default-base-is-moof set.
    let mut tfhd_payload = Vec::with_capacity(8);
    tfhd_payload.extend_from_slice(&TFHD_FLAGS_DEFAULT_BASE_IS_MOOF.to_be_bytes());
    tfhd_payload.extend_from_slice(&1u32.to_be_bytes());
    let tfhd = mp4_box(b"tfhd", &tfhd_payload, "tfhd box")?;

    // tfdt (version 1): version+flags(4) + baseMediaDecodeTime(8).
    let mut tfdt_payload = Vec::with_capacity(12);
    tfdt_payload.extend_from_slice(&0x0100_0000u32.to_be_bytes());
    tfdt_payload.extend_from_slice(&base_decode_time.to_be_bytes());
    let tfdt = mp4_box(b"tfdt", &tfdt_payload, "tfdt box")?;

    // trun with a placeholder data_offset (patched after the moof is sized).
    let trun = mp4_box(
        b"trun",
        &build_trun_payload(sample_count, 0, first_is_sync, &sizes),
        "trun box",
    )?;

    let mut traf_payload = Vec::with_capacity(tfhd.len() + tfdt.len() + trun.len());
    traf_payload.extend_from_slice(&tfhd);
    traf_payload.extend_from_slice(&tfdt);
    traf_payload.extend_from_slice(&trun);
    let traf = mp4_box(b"traf", &traf_payload, "traf box")?;

    let mut moof_payload = Vec::with_capacity(mfhd.len() + traf.len());
    moof_payload.extend_from_slice(&mfhd);
    moof_payload.extend_from_slice(&traf);
    let mut moof = mp4_box(b"moof", &moof_payload, "moof box")?;

    // The mdat header is normally 8 bytes; a >4 GiB fragment needs a 64-bit
    // largesize (16 bytes). data_offset spans only moof + this header.
    let mdat_header_len: u64 = if mdat_payload.len() as u64 > MAX_FRAGMENT_MDAT_U32_PAYLOAD {
        16
    } else {
        8
    };
    let data_offset_u64 =
        (moof.len() as u64)
            .checked_add(mdat_header_len)
            .ok_or(MuxError::SizeOverflow {
                context: "fragment data offset",
            })?;
    let data_offset = i32::try_from(data_offset_u64).map_err(|_| MuxError::SizeOverflow {
        context: "fragment data offset exceeds 32-bit trun field",
    })?;

    // Patch the trun data_offset. Layout inside moof:
    //   [moof hdr 8][mfhd][traf hdr 8][tfhd][tfdt][trun: size 4|type 4|vf 4|count 4|data_offset 4|…]
    let trun_pos = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len();
    debug_assert_eq!(
        &moof[trun_pos + 4..trun_pos + 8],
        b"trun",
        "trun not at the computed offset inside moof"
    );
    let offset_field = trun_pos + 16;
    moof[offset_field..offset_field + 4].copy_from_slice(&data_offset.to_be_bytes());

    let mdat = build_mdat(&mdat_payload)?;
    Ok((moof, mdat))
}

fn build_trun_payload(
    sample_count: u32,
    data_offset: i32,
    first_is_sync: bool,
    sizes: &[u32],
) -> Vec<u8> {
    let first_sample_flags = if first_is_sync {
        SAMPLE_FLAGS_SYNC
    } else {
        SAMPLE_FLAGS_NON_SYNC
    };
    let mut payload = Vec::with_capacity(16usize.saturating_add(sizes.len().saturating_mul(4)));
    payload.extend_from_slice(&TRUN_FLAGS.to_be_bytes());
    payload.extend_from_slice(&sample_count.to_be_bytes());
    payload.extend_from_slice(&data_offset.to_be_bytes());
    payload.extend_from_slice(&first_sample_flags.to_be_bytes());
    for &size in sizes {
        payload.extend_from_slice(&size.to_be_bytes());
    }
    payload
}

fn build_mdat(payload: &[u8]) -> Result<Vec<u8>, MuxError> {
    if payload.len() as u64 > MAX_FRAGMENT_MDAT_U32_PAYLOAD {
        let size = 16u64
            .checked_add(payload.len() as u64)
            .ok_or(MuxError::SizeOverflow {
                context: "fragment mdat largesize",
            })?;
        let mut out = Vec::with_capacity(16 + payload.len());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(payload);
        Ok(out)
    } else {
        let size = box_size_u32(payload.len(), "fragment mdat size")?;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(payload);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(byte: u8, len: usize, is_keyframe: bool) -> MuxFrame {
        MuxFrame {
            av1_data: vec![byte; len],
            is_keyframe,
        }
    }

    /// Two GOPs: keyframe at 0 and 3.
    fn two_gop_frames() -> Vec<MuxFrame> {
        vec![
            frame(0xA0, 5, true),
            frame(0xA1, 2, false),
            frame(0xA2, 3, false),
            frame(0xB0, 4, true),
            frame(0xB1, 2, false),
        ]
    }

    fn cfg() -> Mp4Config {
        Mp4Config::new(24.0, 16, 16)
    }

    /// Walks the top-level box list: returns `(type, start, size)` for each box.
    fn top_level_boxes(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut boxes = Vec::new();
        let mut cursor = 0usize;
        while cursor + 8 <= data.len() {
            let size = u32::from_be_bytes([
                data[cursor],
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
            ]) as usize;
            let mut typ = [0u8; 4];
            typ.copy_from_slice(&data[cursor + 4..cursor + 8]);
            assert!(
                size >= 8 && cursor + size <= data.len(),
                "malformed box {typ:?}"
            );
            boxes.push((typ, cursor, size));
            cursor += size;
        }
        assert_eq!(cursor, data.len(), "trailing bytes after last box");
        boxes
    }

    fn contains_subbox(data: &[u8], typ: &[u8; 4]) -> bool {
        data.windows(4).any(|w| w == typ)
    }

    #[test]
    fn fmp4_top_level_structure_is_init_then_segments() {
        let mut out = Vec::new();
        write_fmp4(&mut out, &cfg(), &two_gop_frames()).expect("fmp4 write must succeed");
        let boxes = top_level_boxes(&out);
        let types: Vec<[u8; 4]> = boxes.iter().map(|b| b.0).collect();

        // ftyp, moov, then (styp, moof, mdat) per segment — two segments here.
        assert_eq!(&types[0], b"ftyp");
        assert_eq!(&types[1], b"moov");
        assert_eq!(&types[2], b"styp");
        assert_eq!(&types[3], b"moof");
        assert_eq!(&types[4], b"mdat");
        assert_eq!(&types[5], b"styp");
        assert_eq!(&types[6], b"moof");
        assert_eq!(&types[7], b"mdat");
        assert_eq!(types.len(), 8, "exactly two media segments expected");
    }

    #[test]
    fn fmp4_moov_declares_fragments_with_empty_sample_tables() {
        let mut out = Vec::new();
        write_fmp4(&mut out, &cfg(), &two_gop_frames()).unwrap();
        let boxes = top_level_boxes(&out);
        let (_, moov_start, moov_size) = boxes.iter().find(|b| &b.0 == b"moov").unwrap();
        let moov = &out[*moov_start..*moov_start + *moov_size];

        assert!(contains_subbox(moov, b"mvex"), "moov must carry mvex");
        assert!(contains_subbox(moov, b"trex"), "moov must carry trex");
        assert!(contains_subbox(moov, b"stsd"), "moov must carry stsd");
        assert!(contains_subbox(moov, b"av01"), "stsd must describe av01");
        assert!(contains_subbox(moov, b"colr"), "stsd must carry colr nclx");

        // stsz present with sample_count 0 (samples live in fragments). Find the
        // stsz box and read its sample_count field (last 4 bytes of a 20-byte box).
        let pos = moov.windows(4).position(|w| w == b"stsz").unwrap();
        let box_start = pos - 4;
        let sample_count = u32::from_be_bytes([
            moov[box_start + 16],
            moov[box_start + 17],
            moov[box_start + 18],
            moov[box_start + 19],
        ]);
        assert_eq!(
            sample_count, 0,
            "init-segment stsz must declare zero samples"
        );
    }

    #[test]
    fn fmp4_trun_data_offset_points_at_first_sample() {
        let frames = two_gop_frames();
        let mut out = Vec::new();
        write_fmp4(&mut out, &cfg(), &frames).unwrap();
        let boxes = top_level_boxes(&out);

        // First moof + its trun data_offset must land on the first sample byte.
        let (_, moof_start, moof_size) = *boxes.iter().find(|b| &b.0 == b"moof").unwrap();
        let moof = &out[moof_start..moof_start + moof_size];
        let trun_rel = moof.windows(4).position(|w| w == b"trun").unwrap() - 4;
        let data_offset = i32::from_be_bytes([
            moof[trun_rel + 16],
            moof[trun_rel + 17],
            moof[trun_rel + 18],
            moof[trun_rel + 19],
        ]);
        let sample_byte_pos = moof_start as i64 + i64::from(data_offset);
        assert_eq!(
            out[sample_byte_pos as usize], 0xA0,
            "data_offset must point at the first GOP's first sample byte"
        );
    }

    #[test]
    fn fmp4_tfdt_decode_time_advances_per_segment() {
        let frames = two_gop_frames();
        let mut out = Vec::new();
        write_fmp4(&mut out, &cfg(), &frames).unwrap();
        let frame_duration = compute_frame_duration(DEFAULT_MP4_TIMESCALE, 24.0).unwrap();

        let moofs: Vec<(usize, usize)> = top_level_boxes(&out)
            .into_iter()
            .filter(|b| &b.0 == b"moof")
            .map(|b| (b.1, b.2))
            .collect();
        assert_eq!(moofs.len(), 2);

        let read_tfdt = |start: usize, size: usize| -> u64 {
            let moof = &out[start..start + size];
            let pos = moof.windows(4).position(|w| w == b"tfdt").unwrap() - 4;
            // version 1: baseMediaDecodeTime is the 8 bytes after size(4)+type(4)+vf(4).
            u64::from_be_bytes(moof[pos + 12..pos + 20].try_into().unwrap())
        };
        assert_eq!(read_tfdt(moofs[0].0, moofs[0].1), 0);
        // Second segment starts at frame index 3.
        assert_eq!(
            read_tfdt(moofs[1].0, moofs[1].1),
            3 * u64::from(frame_duration)
        );
    }

    #[test]
    fn cmaf_advertises_cmaf_brand() {
        let mut out = Vec::new();
        write_cmaf(&mut out, &cfg(), &two_gop_frames()).expect("cmaf write must succeed");
        // major_brand sits at bytes 8..12 of the ftyp box (first box).
        assert_eq!(&out[8..12], b"cmfc", "CMAF init segment major brand");
        assert!(contains_subbox(&out, b"cmfs"), "CMAF styp compatible brand");
    }

    #[test]
    fn single_keyframe_run_is_one_segment() {
        let frames = vec![frame(0x10, 3, true), frame(0x11, 2, false)];
        let mut out = Vec::new();
        write_fmp4(&mut out, &cfg(), &frames).unwrap();
        let moof_count = top_level_boxes(&out)
            .iter()
            .filter(|b| &b.0 == b"moof")
            .count();
        assert_eq!(moof_count, 1);
    }

    #[test]
    fn rejects_empty_and_out_of_range_input() {
        assert!(matches!(
            write_fmp4(&mut Vec::new(), &cfg(), &[]),
            Err(MuxError::NoFrames)
        ));
        let bad_rate = Mp4Config::new(-1.0, 16, 16);
        assert!(write_fmp4(&mut Vec::new(), &bad_rate, &two_gop_frames()).is_err());
        let bad_dim = Mp4Config::new(24.0, 70_000, 16);
        assert!(write_fmp4(&mut Vec::new(), &bad_dim, &two_gop_frames()).is_err());
    }
}
