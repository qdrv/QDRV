// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Minimal ISOBMFF demuxer: recovers the AV1 sample bytes from an MP4.
//!
//! This is the read-side counterpart to [`crate::write_mp4`] /
//! [`crate::write_fmp4`] / [`crate::write_cmaf`]. It walks the container and
//! returns the concatenated AV1 temporal-unit bytes (the samples), so a caller
//! can recover the elementary stream — for example to read QDRV metadata OBUs
//! back out of an exported `.mp4`/fragmented/CMAF file via
//! `qdrv_codec::extract_all_qdrv_metadata`.
//!
//! Both layouts are supported:
//!
//! - **Progressive** — one `mdat` with all samples, located via the
//!   `moov → trak → mdia → minf → stbl` sample tables (`stsz`, `stsc`, and
//!   `stco`/`co64`).
//! - **Fragmented / CMAF** — samples described per fragment in
//!   `moof → traf → trun`, with the byte offset resolved relative to each
//!   `moof` (the default-base-is-moof addressing the writer uses).
//!
//! The demuxer is read-only and never trusts declared sizes: every box range
//! and every sample slice is bounds-checked against the input.

use crate::{MuxError, find_child_box};

fn malformed(message: impl Into<String>) -> MuxError {
    MuxError::Malformed(message.into())
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, MuxError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| malformed("u32 offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| malformed("truncated u32 field"))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, MuxError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| malformed("u64 offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| malformed("truncated u64 field"))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    Ok(u64::from_be_bytes(raw))
}

/// Reads a big-endian `u32` only if the whole field lies within `[.., limit)`,
/// confining a parser's reads to the child box that owns the field rather than
/// the whole file. `limit` is a box end and must be `<= data.len()`.
fn read_u32_in(data: &[u8], offset: usize, limit: usize) -> Result<u32, MuxError> {
    let field_end = offset
        .checked_add(4)
        .ok_or_else(|| malformed("u32 offset overflow"))?;
    if field_end > limit {
        return Err(malformed("field reads past its box"));
    }
    read_u32(data, offset)
}

/// Box-bounded big-endian `u64` read; see [`read_u32_in`].
fn read_u64_in(data: &[u8], offset: usize, limit: usize) -> Result<u64, MuxError> {
    let field_end = offset
        .checked_add(8)
        .ok_or_else(|| malformed("u64 offset overflow"))?;
    if field_end > limit {
        return Err(malformed("field reads past its box"));
    }
    read_u64(data, offset)
}

/// One top-level box: four-character type, its start offset, the offset of its
/// content (after the header), and its exclusive end.
struct TopBox {
    box_type: [u8; 4],
    start: usize,
    end: usize,
}

/// Walks the top-level box list, handling 32-bit sizes, the 64-bit `largesize`
/// escape (`size == 1`), and the to-end-of-file form (`size == 0`).
fn top_level_boxes(data: &[u8]) -> Result<Vec<TopBox>, MuxError> {
    let mut boxes = Vec::new();
    let mut cursor = 0usize;
    while cursor.checked_add(8).is_some_and(|end| end <= data.len()) {
        let size32 = read_u32(data, cursor)?;
        let mut box_type = [0u8; 4];
        box_type.copy_from_slice(&data[cursor + 4..cursor + 8]);

        let box_len = if size32 == 1 {
            usize::try_from(read_u64(data, cursor + 8)?)
                .map_err(|_| malformed("largesize box exceeds usize"))?
        } else if size32 == 0 {
            data.len() - cursor
        } else {
            size32 as usize
        };

        if box_len < 8 {
            return Err(malformed("box size smaller than its header"));
        }
        let end = cursor
            .checked_add(box_len)
            .ok_or_else(|| malformed("box end overflow"))?;
        if end > data.len() {
            return Err(malformed("box runs past end of file"));
        }
        boxes.push(TopBox {
            box_type,
            start: cursor,
            end,
        });
        cursor = end;
    }
    Ok(boxes)
}

/// Extracts and concatenates the AV1 sample bytes (temporal units) from an
/// ISOBMFF file, supporting both progressive and fragmented/CMAF layouts.
///
/// The returned buffer is a valid AV1 elementary stream (the samples back to
/// back, in presentation order as stored), suitable for handing to an OBU-aware
/// reader.
///
/// # Errors
/// [`MuxError::Malformed`] if the file is not parseable ISOBMFF, a required box
/// is missing, or a declared sample range runs past the file.
pub fn extract_av1_samples(data: &[u8]) -> Result<Vec<u8>, MuxError> {
    let boxes = top_level_boxes(data)?;
    if !boxes.iter().any(|b| &b.box_type == b"ftyp") {
        return Err(malformed("not an ISOBMFF file (no ftyp box)"));
    }

    let moofs: Vec<&TopBox> = boxes.iter().filter(|b| &b.box_type == b"moof").collect();
    if !moofs.is_empty() {
        return extract_fragmented(data, &moofs);
    }

    let moov = boxes
        .iter()
        .find(|b| &b.box_type == b"moov")
        .ok_or_else(|| malformed("no moov or moof box"))?;
    extract_progressive(data, moov.start, moov.end)
}

// ---------------------------------------------------------------------------
// Progressive layout (sample tables in moov/stbl)
// ---------------------------------------------------------------------------

fn extract_progressive(
    data: &[u8],
    moov_start: usize,
    moov_end: usize,
) -> Result<Vec<u8>, MuxError> {
    let stbl = locate_stbl(data, moov_start, moov_end)?;
    let (stbl_start, stbl_end) = stbl;

    let (stsz_s, stsz_e) = find_child_box(data, stbl_start + 8, stbl_end, b"stsz")
        .ok_or_else(|| malformed("missing stsz box"))?;
    let (sample_size, sample_count, sizes) = parse_stsz(data, stsz_s, stsz_e)?;

    let (stsc_s, stsc_e) = find_child_box(data, stbl_start + 8, stbl_end, b"stsc")
        .ok_or_else(|| malformed("missing stsc box"))?;
    let stsc = parse_stsc(data, stsc_s, stsc_e)?;

    let chunk_offsets =
        if let Some((co_s, co_e)) = find_child_box(data, stbl_start + 8, stbl_end, b"stco") {
            parse_chunk_offsets(data, co_s, co_e, false)?
        } else if let Some((co_s, co_e)) = find_child_box(data, stbl_start + 8, stbl_end, b"co64") {
            parse_chunk_offsets(data, co_s, co_e, true)?
        } else {
            return Err(malformed("missing stco/co64 box"));
        };

    let mut out = Vec::new();
    let mut sample_index = 0usize;
    for (chunk_idx, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let samples_in_chunk = samples_per_chunk(chunk_idx + 1, &stsc)?;
        let mut offset =
            usize::try_from(chunk_offset).map_err(|_| malformed("chunk offset exceeds usize"))?;
        for _ in 0..samples_in_chunk {
            if sample_index >= sample_count {
                break;
            }
            let size = sample_size_at(sample_index, sample_size, &sizes)? as usize;
            let end = offset
                .checked_add(size)
                .ok_or_else(|| malformed("sample range overflow"))?;
            let slice = data
                .get(offset..end)
                .ok_or_else(|| malformed("sample runs past end of file"))?;
            out.extend_from_slice(slice);
            offset = end;
            sample_index += 1;
        }
    }
    if sample_index != sample_count {
        return Err(malformed(
            "chunk tables describe fewer samples than the stsz sample count",
        ));
    }
    Ok(out)
}

fn locate_stbl(
    data: &[u8],
    moov_start: usize,
    moov_end: usize,
) -> Result<(usize, usize), MuxError> {
    let (trak_s, trak_e) = find_child_box(data, moov_start + 8, moov_end, b"trak")
        .ok_or_else(|| malformed("missing trak box"))?;
    let (mdia_s, mdia_e) = find_child_box(data, trak_s + 8, trak_e, b"mdia")
        .ok_or_else(|| malformed("missing mdia"))?;
    let (minf_s, minf_e) = find_child_box(data, mdia_s + 8, mdia_e, b"minf")
        .ok_or_else(|| malformed("missing minf"))?;
    find_child_box(data, minf_s + 8, minf_e, b"stbl").ok_or_else(|| malformed("missing stbl"))
}

/// Returns `(sample_size, sample_count, per_sample_sizes)`. When `sample_size`
/// is non-zero every sample is that size and `per_sample_sizes` is empty.
fn parse_stsz(
    data: &[u8],
    stsz_start: usize,
    stsz_end: usize,
) -> Result<(u32, usize, Vec<u32>), MuxError> {
    // FullBox: size(4) type(4) version+flags(4), then sample_size(4),
    // sample_count(4), [u32; count?]. Fields start at box_start + 12.
    let base = stsz_start + 12;
    let sample_size = read_u32_in(data, base, stsz_end)?;
    let sample_count = usize::try_from(read_u32_in(data, base + 4, stsz_end)?)
        .map_err(|_| malformed("sample count overflow"))?;
    if sample_size != 0 {
        return Ok((sample_size, sample_count, Vec::new()));
    }
    // The per-sample size table must fit entirely within the stsz box, so a
    // declared count cannot read into sibling boxes or drive work beyond the
    // box's own bytes.
    let table_start = base + 8;
    let table_bytes = sample_count
        .checked_mul(4)
        .ok_or_else(|| malformed("stsz table size overflow"))?;
    let table_end = table_start
        .checked_add(table_bytes)
        .ok_or_else(|| malformed("stsz table end overflow"))?;
    if table_end > stsz_end {
        return Err(malformed("stsz sample table exceeds its box"));
    }
    let mut sizes = Vec::with_capacity(sample_count);
    let mut p = table_start;
    for _ in 0..sample_count {
        sizes.push(read_u32(data, p)?);
        p += 4;
    }
    Ok((0, sample_count, sizes))
}

fn sample_size_at(index: usize, sample_size: u32, sizes: &[u32]) -> Result<u32, MuxError> {
    if sample_size != 0 {
        Ok(sample_size)
    } else {
        sizes
            .get(index)
            .copied()
            .ok_or_else(|| malformed("sample index past stsz table"))
    }
}

/// `stsc` entries as `(first_chunk, samples_per_chunk)` (the sample-description
/// index is not needed to locate bytes).
fn parse_stsc(
    data: &[u8],
    stsc_start: usize,
    stsc_end: usize,
) -> Result<Vec<(u32, u32)>, MuxError> {
    // FullBox: fields start after size(4) + type(4) + version/flags(4).
    let base = stsc_start + 12;
    let count = usize::try_from(read_u32_in(data, base, stsc_end)?)
        .map_err(|_| malformed("stsc entry count overflow"))?;
    // Confine the entry table (12 bytes each) to the stsc box.
    let table_start = base + 4;
    let table_bytes = count
        .checked_mul(12)
        .ok_or_else(|| malformed("stsc table size overflow"))?;
    let table_end = table_start
        .checked_add(table_bytes)
        .ok_or_else(|| malformed("stsc table end overflow"))?;
    if table_end > stsc_end {
        return Err(malformed("stsc table exceeds its box"));
    }
    let mut entries = Vec::with_capacity(count);
    let mut p = table_start;
    for _ in 0..count {
        let first_chunk = read_u32(data, p)?;
        let samples_per_chunk = read_u32(data, p + 4)?;
        // p+8 is sample_description_index, skipped.
        entries.push((first_chunk, samples_per_chunk));
        p += 12;
    }
    if entries.is_empty() {
        return Err(malformed("stsc table is empty"));
    }
    Ok(entries)
}

/// Resolves how many samples live in the 1-based `chunk` using the `stsc` runs.
fn samples_per_chunk(chunk: usize, stsc: &[(u32, u32)]) -> Result<usize, MuxError> {
    let chunk_u32 = u32::try_from(chunk).map_err(|_| malformed("chunk index overflow"))?;
    let mut current = stsc[0].1;
    for &(first_chunk, spc) in stsc {
        if first_chunk <= chunk_u32 {
            current = spc;
        } else {
            break;
        }
    }
    usize::try_from(current).map_err(|_| malformed("samples-per-chunk overflow"))
}

fn parse_chunk_offsets(
    data: &[u8],
    box_start: usize,
    box_end: usize,
    is_co64: bool,
) -> Result<Vec<u64>, MuxError> {
    // FullBox: entry_count starts after size(4) + type(4) + version/flags(4).
    let base = box_start + 12;
    let count = usize::try_from(read_u32_in(data, base, box_end)?)
        .map_err(|_| malformed("chunk offset count overflow"))?;
    // Confine the offset table (4 or 8 bytes each) to the stco/co64 box.
    let entry_size = if is_co64 { 8 } else { 4 };
    let table_start = base + 4;
    let table_bytes = count
        .checked_mul(entry_size)
        .ok_or_else(|| malformed("chunk offset table size overflow"))?;
    let table_end = table_start
        .checked_add(table_bytes)
        .ok_or_else(|| malformed("chunk offset table end overflow"))?;
    if table_end > box_end {
        return Err(malformed("chunk offset table exceeds its box"));
    }
    let mut offsets = Vec::with_capacity(count);
    let mut p = table_start;
    for _ in 0..count {
        if is_co64 {
            offsets.push(read_u64(data, p)?);
            p += 8;
        } else {
            offsets.push(u64::from(read_u32(data, p)?));
            p += 4;
        }
    }
    Ok(offsets)
}

// ---------------------------------------------------------------------------
// Fragmented / CMAF layout (samples described in moof/traf/trun)
// ---------------------------------------------------------------------------

// trun flags (ISO/IEC 14496-12 §8.8.8).
const TRUN_DATA_OFFSET_PRESENT: u32 = 0x000001;
const TRUN_FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x000004;
const TRUN_SAMPLE_DURATION_PRESENT: u32 = 0x000100;
const TRUN_SAMPLE_SIZE_PRESENT: u32 = 0x000200;
const TRUN_SAMPLE_FLAGS_PRESENT: u32 = 0x000400;
const TRUN_SAMPLE_CTO_PRESENT: u32 = 0x000800;

// tfhd flags (ISO/IEC 14496-12 §8.8.7).
const TFHD_BASE_DATA_OFFSET_PRESENT: u32 = 0x000001;
const TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT: u32 = 0x000002;
const TFHD_DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x000008;
const TFHD_DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x000010;

fn extract_fragmented(data: &[u8], moofs: &[&TopBox]) -> Result<Vec<u8>, MuxError> {
    let mut out = Vec::new();
    for moof in moofs {
        let (traf_s, traf_e) = find_child_box(data, moof.start + 8, moof.end, b"traf")
            .ok_or_else(|| malformed("moof without traf"))?;

        let (base_offset, default_sample_size) = parse_tfhd(data, traf_s, traf_e, moof.start)?;

        let (trun_s, trun_e) = find_child_box(data, traf_s + 8, traf_e, b"trun")
            .ok_or_else(|| malformed("traf without trun"))?;
        extract_trun_samples(
            data,
            trun_s,
            trun_e,
            base_offset,
            default_sample_size,
            &mut out,
        )?;
    }
    Ok(out)
}

/// Returns `(base_offset, default_sample_size)` for the fragment. `base_offset`
/// is where the `trun` `data_offset` is measured from: the explicit
/// base-data-offset if present, otherwise the enclosing `moof` start
/// (default-base-is-moof, which the writer uses).
fn parse_tfhd(
    data: &[u8],
    traf_start: usize,
    traf_end: usize,
    moof_start: usize,
) -> Result<(u64, Option<u32>), MuxError> {
    let Some((tfhd_s, tfhd_e)) = find_child_box(data, traf_start + 8, traf_end, b"tfhd") else {
        // A tfhd is mandatory in every traf (ISO/IEC 14496-12); a fragment that
        // omits it is malformed input rather than a defaultable condition.
        return Err(malformed("traf is missing its mandatory tfhd box"));
    };
    // The box must hold its header (8), the version/flags word (4), and the
    // mandatory track_ID (4) before any optional field: 16 bytes at minimum.
    if tfhd_e - tfhd_s < 16 {
        return Err(malformed("tfhd is too small for its mandatory track_ID"));
    }
    let flags = read_u32_in(data, tfhd_s + 8, tfhd_e)? & 0x00FF_FFFF;
    // content after version+flags(4) and track_ID(4).
    let mut p = tfhd_s + 16;
    // Consumes the next `n` bytes of an optional tfhd field, confirming they
    // fall inside the tfhd box before advancing. Optional fields whose value we
    // skip must still be validated against the box, not merely stepped over.
    let consume = |p: usize, n: usize| -> Result<usize, MuxError> {
        let end = p
            .checked_add(n)
            .ok_or_else(|| malformed("tfhd field offset overflow"))?;
        if end > tfhd_e {
            return Err(malformed("tfhd optional field runs past its box"));
        }
        Ok(end)
    };
    let mut base_offset = moof_start as u64;
    if flags & TFHD_BASE_DATA_OFFSET_PRESENT != 0 {
        base_offset = read_u64_in(data, p, tfhd_e)?;
        p = consume(p, 8)?;
    }
    if flags & TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT != 0 {
        p = consume(p, 4)?;
    }
    if flags & TFHD_DEFAULT_SAMPLE_DURATION_PRESENT != 0 {
        p = consume(p, 4)?;
    }
    let mut default_sample_size = None;
    if flags & TFHD_DEFAULT_SAMPLE_SIZE_PRESENT != 0 {
        default_sample_size = Some(read_u32_in(data, p, tfhd_e)?);
    }
    Ok((base_offset, default_sample_size))
}

fn extract_trun_samples(
    data: &[u8],
    trun_start: usize,
    trun_end: usize,
    base_offset: u64,
    default_sample_size: Option<u32>,
    out: &mut Vec<u8>,
) -> Result<(), MuxError> {
    let flags = read_u32_in(data, trun_start + 8, trun_end)? & 0x00FF_FFFF;
    let sample_count = usize::try_from(read_u32_in(data, trun_start + 12, trun_end)?)
        .map_err(|_| malformed("trun count"))?;
    // A sample stream cannot contain more samples than the file has bytes; this
    // bounds the loop even when no per-sample fields are present (per_sample = 0).
    if sample_count > data.len() {
        return Err(malformed("trun sample_count exceeds file size"));
    }
    let mut p = trun_start + 16;

    let mut data_offset: i64 = 0;
    if flags & TRUN_DATA_OFFSET_PRESENT != 0 {
        data_offset = i64::from(read_u32_in(data, p, trun_end)? as i32);
        p += 4;
    }
    if flags & TRUN_FIRST_SAMPLE_FLAGS_PRESENT != 0 {
        p += 4;
    }

    // Confine the per-sample field table to the trun box: derive the per-sample
    // record size from the flags and verify the whole table fits before looping,
    // so a declared sample_count cannot read fields out of sibling boxes.
    let mut per_sample_bytes = 0usize;
    if flags & TRUN_SAMPLE_DURATION_PRESENT != 0 {
        per_sample_bytes += 4;
    }
    if flags & TRUN_SAMPLE_SIZE_PRESENT != 0 {
        per_sample_bytes += 4;
    }
    if flags & TRUN_SAMPLE_FLAGS_PRESENT != 0 {
        per_sample_bytes += 4;
    }
    if flags & TRUN_SAMPLE_CTO_PRESENT != 0 {
        per_sample_bytes += 4;
    }
    let table_bytes = sample_count
        .checked_mul(per_sample_bytes)
        .ok_or_else(|| malformed("trun table size overflow"))?;
    let table_end = p
        .checked_add(table_bytes)
        .ok_or_else(|| malformed("trun table end overflow"))?;
    if table_end > trun_end {
        return Err(malformed("trun sample table exceeds its box"));
    }

    let base_signed =
        i64::try_from(base_offset).map_err(|_| malformed("base data offset exceeds i64"))?;
    let sample_base = base_signed
        .checked_add(data_offset)
        .ok_or_else(|| malformed("fragment sample base overflow"))?;
    if sample_base < 0 {
        return Err(malformed("negative fragment sample offset"));
    }
    let mut offset =
        usize::try_from(sample_base).map_err(|_| malformed("sample offset overflow"))?;

    for _ in 0..sample_count {
        if flags & TRUN_SAMPLE_DURATION_PRESENT != 0 {
            p += 4;
        }
        let size = if flags & TRUN_SAMPLE_SIZE_PRESENT != 0 {
            let s = read_u32(data, p)?;
            p += 4;
            s
        } else {
            default_sample_size
                .ok_or_else(|| malformed("trun without per-sample size and no tfhd default"))?
        } as usize;
        if flags & TRUN_SAMPLE_FLAGS_PRESENT != 0 {
            p += 4;
        }
        if flags & TRUN_SAMPLE_CTO_PRESENT != 0 {
            p += 4;
        }

        let end = offset
            .checked_add(size)
            .ok_or_else(|| malformed("fragment sample range overflow"))?;
        let slice = data
            .get(offset..end)
            .ok_or_else(|| malformed("fragment sample runs past end of file"))?;
        out.extend_from_slice(slice);
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mp4Config, MuxFrame, write_cmaf, write_fmp4, write_mp4};

    fn sample_frames() -> Vec<MuxFrame> {
        // Distinct, identifiable sample payloads with keyframes at 0 and 2 so
        // the fragmented writer produces more than one media segment.
        vec![
            MuxFrame {
                av1_data: vec![0xA0; 5],
                is_keyframe: true,
            },
            MuxFrame {
                av1_data: vec![0xA1; 3],
                is_keyframe: false,
            },
            MuxFrame {
                av1_data: vec![0xB0; 7],
                is_keyframe: true,
            },
            MuxFrame {
                av1_data: vec![0xB1; 2],
                is_keyframe: false,
            },
        ]
    }

    fn expected_concatenation(frames: &[MuxFrame]) -> Vec<u8> {
        frames.iter().flat_map(|f| f.av1_data.clone()).collect()
    }

    #[test]
    fn demux_progressive_recovers_samples_in_order() {
        let frames = sample_frames();
        let mut mp4 = Vec::new();
        write_mp4(&mut mp4, &Mp4Config::new(24.0, 16, 16), &frames).unwrap();
        let samples = extract_av1_samples(&mp4).expect("progressive demux must succeed");
        assert_eq!(samples, expected_concatenation(&frames));
    }

    #[test]
    fn demux_fragmented_recovers_samples_across_segments() {
        let frames = sample_frames();
        let mut fmp4 = Vec::new();
        write_fmp4(&mut fmp4, &Mp4Config::new(24.0, 16, 16), &frames).unwrap();
        let samples = extract_av1_samples(&fmp4).expect("fragmented demux must succeed");
        assert_eq!(samples, expected_concatenation(&frames));
    }

    #[test]
    fn demux_cmaf_recovers_samples() {
        let frames = sample_frames();
        let mut cmaf = Vec::new();
        write_cmaf(&mut cmaf, &Mp4Config::new(24.0, 16, 16), &frames).unwrap();
        let samples = extract_av1_samples(&cmaf).expect("cmaf demux must succeed");
        assert_eq!(samples, expected_concatenation(&frames));
    }

    #[test]
    fn demux_rejects_non_isobmff() {
        let err = extract_av1_samples(&[0u8; 32]).unwrap_err();
        assert!(matches!(err, MuxError::Malformed(_)));
    }

    #[test]
    fn demux_rejects_truncated_box() {
        // ftyp claiming a size far larger than the buffer.
        let mut data = Vec::new();
        data.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        assert!(extract_av1_samples(&data).is_err());
    }

    /// M-01 regression: a declared sample-table count larger than the box can
    /// hold must be rejected, not trusted into reads past the box / whole file.
    #[test]
    fn demux_rejects_oversized_stsz_count() {
        let frames = sample_frames();
        let mut mp4 = Vec::new();
        write_mp4(&mut mp4, &Mp4Config::new(24.0, 16, 16), &frames).unwrap();
        // stsz layout: [size 4][type 4 @pos][version+flags 4][sample_size 4][sample_count 4].
        let pos = mp4
            .windows(4)
            .position(|w| w == b"stsz")
            .expect("stsz present");
        let count_off = pos + 12; // box_start(pos-4) + 16 → sample_count
        mp4[count_off..count_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        let err = extract_av1_samples(&mp4).unwrap_err();
        assert!(matches!(err, MuxError::Malformed(_)), "got {err:?}");
    }

    /// L-02 regression: if the chunk tables cover fewer samples than `stsz`
    /// declares, the demuxer must reject the file rather than return a
    /// silently-truncated elementary stream.
    #[test]
    fn demux_rejects_chunk_table_shorter_than_stsz_count() {
        let frames = sample_frames();
        let mut mp4 = Vec::new();
        write_mp4(&mut mp4, &Mp4Config::new(24.0, 16, 16), &frames).unwrap();
        // stco layout: [size 4][type 4 @pos][version+flags 4][entry_count 4][offsets..].
        // Zeroing entry_count makes the chunk table describe zero samples while
        // stsz still declares four.
        let pos = mp4
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco present");
        let count_off = pos + 8; // box_start(pos-4) + 12 → entry_count
        mp4[count_off..count_off + 4].copy_from_slice(&0u32.to_be_bytes());
        let err = extract_av1_samples(&mp4).unwrap_err();
        assert!(matches!(err, MuxError::Malformed(_)), "got {err:?}");
    }

    /// L-03 regression: a traf that omits its mandatory tfhd box is malformed
    /// input and must be rejected rather than silently defaulted.
    #[test]
    fn demux_rejects_fragment_without_tfhd() {
        let frames = sample_frames();
        let mut fmp4 = Vec::new();
        write_fmp4(&mut fmp4, &Mp4Config::new(24.0, 16, 16), &frames).unwrap();
        // Rename the first tfhd so find_child_box can no longer locate it.
        let pos = fmp4
            .windows(4)
            .position(|w| w == b"tfhd")
            .expect("tfhd present");
        fmp4[pos..pos + 4].copy_from_slice(b"zfhd");
        let err = extract_av1_samples(&fmp4).unwrap_err();
        assert!(matches!(err, MuxError::Malformed(_)), "got {err:?}");
    }

    /// L-01 regression: a tfhd that flags an optional field (here
    /// sample_description_index) but whose box ends before that field must be
    /// rejected, not silently stepped over.
    #[test]
    fn demux_rejects_tfhd_optional_field_past_box() {
        // tfhd holding only version/flags + track_ID (16 bytes), yet flagging
        // sample_description_index present — the field has no room in the box.
        let mut tfhd = Vec::new();
        tfhd.extend_from_slice(&16u32.to_be_bytes()); // box size
        tfhd.extend_from_slice(b"tfhd");
        // version 0, flags = TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT (0x000002).
        tfhd.extend_from_slice(&0x0000_0002u32.to_be_bytes());
        tfhd.extend_from_slice(&1u32.to_be_bytes()); // track_ID
        assert_eq!(tfhd.len(), 16);

        let mut traf = Vec::new();
        let traf_size = u32::try_from(8 + tfhd.len()).unwrap();
        traf.extend_from_slice(&traf_size.to_be_bytes());
        traf.extend_from_slice(b"traf");
        traf.extend_from_slice(&tfhd);

        let err = parse_tfhd(&traf, 0, traf.len(), 0).unwrap_err();
        assert!(matches!(err, MuxError::Malformed(_)), "got {err:?}");
    }
}
