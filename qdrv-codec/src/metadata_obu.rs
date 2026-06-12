// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! QDRV dynamic-metadata carriage as AV1 metadata OBUs (ITU-T T.35).
//!
//! QDRV's per-frame dynamic metadata lives natively in the `.qdrv` container,
//! but when a delivery stream is exported to a *standard* container — MP4,
//! fragmented MP4, CMAF, IVF, or a bare OBU stream — none of those carry the
//! QDRV metadata on their own. Rather than bolt a sidecar onto each target,
//! this module embeds the metadata **inside the AV1 elementary stream** as an
//! `OBU_METADATA` of type `METADATA_TYPE_ITUT_T35`. Because it rides in the
//! bitstream, every container target carries it uniformly and any AV1 decoder
//! that does not recognise it skips it as opaque user data (the same mechanism
//! HDR10+ and Dolby Vision use to ride inside AV1/HEVC).
//!
//! ## QDRV T.35 framing
//!
//! The metadata OBU payload is:
//!
//! ```text
//! leb128(metadata_type = 4)        // METADATA_TYPE_ITUT_T35
//! u8(itu_t_t35_country_code = 0xB5) // United States registered space
//! "QDRV"                            // 4-byte QDRV magic (provider marker)
//! u8(version = 1)                   // QDRV T.35 framing version
//! <opaque QDRV payload bytes>       // caller-supplied (e.g. encode_dynamic_binary)
//! ```
//!
//! The payload after the magic is treated as **opaque** here: this module is
//! codec-layer and metadata-agnostic, so the caller decides what to serialise
//! into it (the QDRV muxer passes the compact binary dynamic-metadata blob).
//!
//! The OBU is inserted immediately before the first frame (or frame-header) OBU
//! of the temporal unit, so it applies to that frame, after any temporal
//! delimiter and sequence header.

use crate::error::CodecError;

/// `OBU_METADATA` header byte: `obu_type = 5`, `obu_has_size_field = 1`, no
/// extension. (`0b0_0101_0_1_0`.)
const OBU_METADATA_HEADER_BYTE: u8 = 0x2A;

const OBU_TYPE_FRAME_HEADER: u8 = 3;
const OBU_TYPE_METADATA: u8 = 5;
const OBU_TYPE_FRAME: u8 = 6;

/// `METADATA_TYPE_ITUT_T35` from the AV1 specification.
const METADATA_TYPE_ITUT_T35: u8 = 4;
/// ITU-T T.35 country code for the United States — the registered space used
/// for carriage of user-defined data in AV1 (as HDR10+/DV do).
const T35_COUNTRY_CODE_USA: u8 = 0xB5;
/// QDRV provider marker that follows the country code, so a QDRV-aware reader
/// can distinguish our user data from any other T.35 payload.
const QDRV_T35_MAGIC: &[u8; 4] = b"QDRV";
/// Version of the QDRV T.35 framing (not the metadata payload's own schema).
const QDRV_T35_VERSION: u8 = 1;
/// AV1 metadata trailing marker. Conformant `OBU_METADATA` payloads end with a
/// `0x80` marker byte (a set bit followed by zero padding to byte-align);
/// decoders such as dav1d trim trailing zeros and then discard this marker
/// while parsing the message, so omitting it triggers a "malformed ITU-T T.35"
/// diagnostic even though the frame still decodes. We append it on embed and
/// strip it on extract.
const T35_TRAILING_MARKER: u8 = 0x80;

fn obu_error(message: impl Into<String>) -> CodecError {
    CodecError::MetadataObu(message.into())
}

/// Appends `value` as an unsigned LEB128 integer (AV1 §4.10.5, max 8 bytes).
fn write_leb128(mut value: u64, out: &mut Vec<u8>) {
    for _ in 0..8 {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Reads an unsigned LEB128 integer at `offset`, returning `(value, len)`.
/// AV1 caps LEB128 at 8 bytes.
fn read_leb128(data: &[u8], offset: usize) -> Result<(u64, usize), CodecError> {
    let mut value: u64 = 0;
    for i in 0..8 {
        let index = offset
            .checked_add(i)
            .ok_or_else(|| obu_error("LEB128 offset overflow"))?;
        let byte = *data
            .get(index)
            .ok_or_else(|| obu_error("truncated LEB128 value"))?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(obu_error("LEB128 value exceeds the 8-byte AV1 maximum"))
}

/// One parsed OBU within a temporal unit.
struct ObuRecord {
    obu_type: u8,
    payload_offset: usize,
    payload_len: usize,
    /// Byte length of the whole OBU (header + size field + payload).
    total: usize,
}

/// Parses the OBU beginning at `offset`. Handles both the low-overhead format
/// (size field present, as rav1e emits) and a trailing sizeless OBU.
fn parse_obu_at(data: &[u8], offset: usize) -> Result<ObuRecord, CodecError> {
    let header = *data
        .get(offset)
        .ok_or_else(|| obu_error("truncated OBU header"))?;
    if header & 0x80 != 0 {
        return Err(obu_error("obu_forbidden_bit is set"));
    }
    let obu_type = (header >> 3) & 0x0f;
    let has_extension = (header >> 2) & 1 == 1;
    let has_size_field = (header >> 1) & 1 == 1;

    let mut header_len = 1usize;
    if has_extension {
        let ext = offset
            .checked_add(1)
            .ok_or_else(|| obu_error("OBU extension offset overflow"))?;
        data.get(ext)
            .ok_or_else(|| obu_error("truncated OBU extension header"))?;
        header_len += 1;
    }

    if has_size_field {
        let header_end = offset
            .checked_add(header_len)
            .ok_or_else(|| obu_error("OBU header offset overflow"))?;
        let (size, leb_len) = read_leb128(data, header_end)?;
        let size = usize::try_from(size).map_err(|_| obu_error("OBU size exceeds usize"))?;
        let payload_offset = header_end
            .checked_add(leb_len)
            .ok_or_else(|| obu_error("OBU offset arithmetic overflow"))?;
        let total = payload_offset
            .checked_add(size)
            .ok_or_else(|| obu_error("OBU size arithmetic overflow"))?;
        if total > data.len() {
            return Err(obu_error("OBU size runs past the temporal unit"));
        }
        Ok(ObuRecord {
            obu_type,
            payload_offset,
            payload_len: size,
            total,
        })
    } else {
        // No size field: the OBU occupies the rest of the temporal unit.
        let payload_offset = offset
            .checked_add(header_len)
            .ok_or_else(|| obu_error("OBU offset arithmetic overflow"))?;
        if payload_offset > data.len() {
            return Err(obu_error("truncated sizeless OBU"));
        }
        Ok(ObuRecord {
            obu_type,
            payload_offset,
            payload_len: data.len() - payload_offset,
            total: data.len(),
        })
    }
}

/// Returns the byte offset of the first frame (or frame-header) OBU, which is
/// where a per-frame metadata OBU must be inserted so it applies to that frame.
/// If the temporal unit carries no frame OBU, returns its length (append).
fn frame_obu_offset(temporal_unit: &[u8]) -> Result<usize, CodecError> {
    let mut offset = 0usize;
    while offset < temporal_unit.len() {
        let record = parse_obu_at(temporal_unit, offset)?;
        if record.obu_type == OBU_TYPE_FRAME || record.obu_type == OBU_TYPE_FRAME_HEADER {
            return Ok(offset);
        }
        offset = record.total;
    }
    Ok(temporal_unit.len())
}

/// Builds a complete QDRV ITU-T T.35 `OBU_METADATA` wrapping `payload`.
fn build_metadata_obu(payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut obu_payload = Vec::with_capacity(8 + payload.len());
    write_leb128(u64::from(METADATA_TYPE_ITUT_T35), &mut obu_payload);
    obu_payload.push(T35_COUNTRY_CODE_USA);
    obu_payload.extend_from_slice(QDRV_T35_MAGIC);
    obu_payload.push(QDRV_T35_VERSION);
    obu_payload.extend_from_slice(payload);
    obu_payload.push(T35_TRAILING_MARKER);

    // AV1 obu_size is a LEB128 that must fit a u32 in practice; reject anything
    // that could not be expressed (a multi-gigabyte metadata blob is a caller
    // bug, not a stream we should emit).
    if u32::try_from(obu_payload.len()).is_err() {
        return Err(obu_error("metadata OBU payload exceeds u32 size"));
    }

    let mut obu = Vec::with_capacity(2 + obu_payload.len());
    obu.extend_from_slice(&[OBU_METADATA_HEADER_BYTE]);
    write_leb128(obu_payload.len() as u64, &mut obu);
    obu.extend_from_slice(&obu_payload);
    Ok(obu)
}

/// Embeds `payload` as a QDRV ITU-T T.35 metadata OBU inside `temporal_unit`,
/// returning a new temporal unit with the OBU spliced in before the frame data.
///
/// The frame OBUs and their sizes are untouched, so the result decodes
/// identically on any AV1 decoder — the metadata OBU is opaque to decoders that
/// do not recognise the QDRV provider marker.
///
/// # Errors
/// [`CodecError::MetadataObu`] if `temporal_unit` is not a parseable AV1 OBU
/// sequence, or if `payload` is too large to express as an OBU size.
pub fn embed_qdrv_metadata(temporal_unit: &[u8], payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    let metadata_obu = build_metadata_obu(payload)?;
    let insert_at = frame_obu_offset(temporal_unit)?;
    let mut out = Vec::with_capacity(temporal_unit.len() + metadata_obu.len());
    out.extend_from_slice(&temporal_unit[..insert_at]);
    out.extend_from_slice(&metadata_obu);
    out.extend_from_slice(&temporal_unit[insert_at..]);
    Ok(out)
}

/// Extracts the QDRV metadata payload from a temporal unit, if present.
///
/// Walks the OBUs, and for each `OBU_METADATA` of ITU-T T.35 type bearing the
/// QDRV magic and a recognised framing version, returns the opaque payload
/// bytes (the inverse of [`embed_qdrv_metadata`]). Returns `Ok(None)` if the
/// temporal unit carries no QDRV metadata OBU.
///
/// # Errors
/// [`CodecError::MetadataObu`] if the temporal unit is not parseable.
pub fn extract_qdrv_metadata(temporal_unit: &[u8]) -> Result<Option<Vec<u8>>, CodecError> {
    let mut offset = 0usize;
    while offset < temporal_unit.len() {
        let record = parse_obu_at(temporal_unit, offset)?;
        if record.obu_type == OBU_TYPE_METADATA {
            let body =
                &temporal_unit[record.payload_offset..record.payload_offset + record.payload_len];
            if let Some(payload) = parse_qdrv_t35(body) {
                return Ok(Some(payload));
            }
        }
        offset = record.total;
    }
    Ok(None)
}

/// Extracts **every** QDRV metadata payload from a multi-frame AV1 stream, in
/// stream order.
///
/// Unlike [`extract_qdrv_metadata`] (which returns the first match in a single
/// temporal unit), this walks an entire concatenated elementary stream — e.g.
/// a raw `.obu` export holding many temporal units — and returns one payload
/// per QDRV metadata OBU found. The result length is the number of frames whose
/// metadata was carried.
///
/// # Errors
/// [`CodecError::MetadataObu`] if the stream is not a parseable OBU sequence.
pub fn extract_all_qdrv_metadata(stream: &[u8]) -> Result<Vec<Vec<u8>>, CodecError> {
    let mut found = Vec::new();
    let mut offset = 0usize;
    while offset < stream.len() {
        let record = parse_obu_at(stream, offset)?;
        if record.obu_type == OBU_TYPE_METADATA {
            let body = &stream[record.payload_offset..record.payload_offset + record.payload_len];
            if let Some(payload) = parse_qdrv_t35(body) {
                found.push(payload);
            }
        }
        offset = record.total;
    }
    Ok(found)
}

/// Parses a metadata-OBU body, returning the QDRV payload only if the body is a
/// QDRV-framed ITU-T T.35 message.
fn parse_qdrv_t35(body: &[u8]) -> Option<Vec<u8>> {
    let (metadata_type, consumed) = read_leb128(body, 0).ok()?;
    if metadata_type != u64::from(METADATA_TYPE_ITUT_T35) {
        return None;
    }
    let rest = body.get(consumed..)?;
    if *rest.first()? != T35_COUNTRY_CODE_USA {
        return None;
    }
    let rest = rest.get(1..)?;
    if rest.len() < QDRV_T35_MAGIC.len() + 1 || &rest[..QDRV_T35_MAGIC.len()] != QDRV_T35_MAGIC {
        return None;
    }
    if rest[QDRV_T35_MAGIC.len()] != QDRV_T35_VERSION {
        return None;
    }
    // Strip the trailing AV1 metadata marker appended on embed.
    let payload = rest[QDRV_T35_MAGIC.len() + 1..].strip_suffix(&[T35_TRAILING_MARKER])?;
    Some(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_round_trips_representative_values() {
        for value in [0u64, 1, 4, 127, 128, 255, 300, 16_383, 16_384, 1_000_000] {
            let mut buf = Vec::new();
            write_leb128(value, &mut buf);
            let (decoded, len) = read_leb128(&buf, 0).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(len, buf.len());
        }
    }

    /// Builds a minimal synthetic temporal unit: temporal delimiter, sequence
    /// header, then a frame OBU — each in the low-overhead (size-field) form.
    // OBU header bytes (obu_type in bits 6..3, obu_has_size_field in bit 1).
    const HDR_TEMPORAL_DELIMITER: u8 = 0x12; // type 2, has_size
    const HDR_SEQUENCE_HEADER: u8 = 0x0A; // type 1, has_size
    const HDR_FRAME: u8 = 0x32; // type 6, has_size

    fn synthetic_temporal_unit() -> Vec<u8> {
        let mut tu = Vec::new();
        // Temporal delimiter, empty payload.
        tu.extend_from_slice(&[HDR_TEMPORAL_DELIMITER, 0]);
        // Sequence header, 3-byte dummy payload.
        tu.extend_from_slice(&[HDR_SEQUENCE_HEADER, 3, 0xAA, 0xBB, 0xCC]);
        // Frame OBU, 4-byte dummy payload.
        tu.extend_from_slice(&[HDR_FRAME, 4, 0x11, 0x22, 0x33, 0x44]);
        tu
    }

    #[test]
    fn embed_inserts_metadata_before_the_frame_obu() {
        let tu = synthetic_temporal_unit();
        let frame_off = frame_obu_offset(&tu).unwrap();
        let embedded = embed_qdrv_metadata(&tu, b"hello").unwrap();

        // The metadata OBU header byte must appear exactly at the old frame
        // offset, and the frame OBU header must follow it.
        assert_eq!(embedded[frame_off], OBU_METADATA_HEADER_BYTE);
        let new_frame_off = frame_obu_offset(&embedded).unwrap();
        assert!(
            new_frame_off > frame_off,
            "frame OBU must shift after the metadata"
        );
        assert_eq!(
            embedded[new_frame_off], HDR_FRAME,
            "frame OBU header byte (type 6, has_size)"
        );

        // Everything before the insertion point (TD + sequence header) is intact.
        assert_eq!(&embedded[..frame_off], &tu[..frame_off]);
    }

    #[test]
    fn embed_then_extract_round_trips_the_payload() {
        let tu = synthetic_temporal_unit();
        let payload = b"qdrv-dynamic-metadata-binary-blob";
        let embedded = embed_qdrv_metadata(&tu, payload).unwrap();
        let extracted = extract_qdrv_metadata(&embedded).unwrap();
        assert_eq!(extracted.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn extract_returns_none_without_qdrv_metadata() {
        let tu = synthetic_temporal_unit();
        assert_eq!(extract_qdrv_metadata(&tu).unwrap(), None);
    }

    #[test]
    fn extract_all_collects_every_unit_in_order() {
        // Two temporal units, each embedding a distinct payload, concatenated
        // as a raw OBU elementary stream would be.
        let mut stream = embed_qdrv_metadata(&synthetic_temporal_unit(), b"first").unwrap();
        stream.extend_from_slice(
            &embed_qdrv_metadata(&synthetic_temporal_unit(), b"second").unwrap(),
        );
        let all = extract_all_qdrv_metadata(&stream).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], b"first");
        assert_eq!(all[1], b"second");
    }

    #[test]
    fn extract_ignores_non_qdrv_t35_metadata() {
        // A T.35 metadata OBU with a different country code must be skipped.
        let mut body = Vec::new();
        write_leb128(u64::from(METADATA_TYPE_ITUT_T35), &mut body);
        body.push(0x26); // some other country code
        body.extend_from_slice(b"\x00\x3C\x00\x01other");
        let mut obu = vec![OBU_METADATA_HEADER_BYTE];
        write_leb128(body.len() as u64, &mut obu);
        obu.extend_from_slice(&body);
        assert_eq!(extract_qdrv_metadata(&obu).unwrap(), None);
    }

    #[test]
    fn malformed_temporal_unit_is_rejected() {
        // Header claims a size-field but the size runs past the buffer.
        let bad = vec![OBU_METADATA_HEADER_BYTE, 0x7F];
        assert!(extract_qdrv_metadata(&bad).is_err());
    }

    #[test]
    fn empty_payload_round_trips() {
        let tu = synthetic_temporal_unit();
        let embedded = embed_qdrv_metadata(&tu, b"").unwrap();
        assert_eq!(extract_qdrv_metadata(&embedded).unwrap(), Some(Vec::new()));
    }

    /// End-to-end proof against real rav1e/dav1d: embedding the metadata OBU
    /// must not perturb decoding (identical pixels), and the payload must
    /// extract back out intact.
    #[test]
    fn embedded_stream_decodes_identically_and_metadata_round_trips() {
        use crate::{Av1Config, av1_decode, av1_encode};
        use qdrv_core::pixel::Pixel32;

        let cfg = Av1Config {
            quantizer: 0,
            lossless: true,
            speed: 10,
            threads: 1,
            ..Default::default()
        };
        let (w, h) = (16u32, 8u32);
        let pixels = vec![Pixel32::new_unchecked(0.3, 0.55, 0.72); (w * h) as usize];
        let temporal_unit = av1_encode(&pixels, w, h, &cfg).expect("encode must succeed");
        let baseline = av1_decode(&temporal_unit, w, h).expect("baseline decode must succeed");

        let payload = b"qdrv-t35-roundtrip-payload";
        let embedded = embed_qdrv_metadata(&temporal_unit, payload).expect("embed must succeed");
        assert!(embedded.len() > temporal_unit.len());

        let decoded = av1_decode(&embedded, w, h).expect("embedded stream must still decode");
        assert_eq!(decoded.len(), baseline.len());
        for (got, want) in decoded.iter().zip(&baseline) {
            assert_eq!(got.r, want.r);
            assert_eq!(got.g, want.g);
            assert_eq!(got.b, want.b);
        }

        assert_eq!(
            extract_qdrv_metadata(&embedded).unwrap().as_deref(),
            Some(payload.as_slice())
        );
    }
}
