// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Compact binary encoding for QDRV metadata.
//!
//! JSON metadata is human-readable but incurs serialisation overhead at high
//! frame rates. This module provides a fixed-layout little-endian binary
//! encoding for both [`StaticMeta`] and [`DynamicMeta`] that can be read and
//! written in constant time with no allocation.
//!
//! ## StaticMeta binary layout (128 bytes)
//!
//! | Offset | Size | Type | Field |
//! |--------|------|------|-------|
//! | 0 | 8 | `[u8; 8]` | Magic: `QDRVMETA` |
//! | 8 | 1 | `u8` | Tier (0 = mastering, 1 = delivery) |
//! | 9 | 1 | `u8` | Precision (0 = float64, 1 = float32) |
//! | 10 | 1 | `u8` | Chroma subsampling (0 = 4:4:4, 1 = 4:2:0) |
//! | 11 | 1 | `u8` | Reserved (0) |
//! | 12 | 48 | 6×f64 | Mastering display primaries (Rx, Ry, Gx, Gy, Bx, By) |
//! | 60 | 16 | 2×f64 | White point (Wx, Wy) |
//! | 76 | 16 | 2×f64 | Min/max luminance nits |
//! | 92 | 8 | 2×f32 | Content light level (MaxCLL, MaxFALL) |
//! | 100 | 28 | — | Reserved (zeros) |
//!
//! ## DynamicMeta binary layout (variable, 24 + 8 × anchor_count bytes)
//!
//! | Offset | Size | Type | Field |
//! |--------|------|------|-------|
//! | 0 | 8 | `u64 LE` | Frame index |
//! | 8 | 4 | `f32 LE` | Scene peak luminance nits |
//! | 12 | 4 | `f32 LE` | Scene average luminance nits |
//! | 16 | 1 | `u8` | Curve type (0 = bezier, 1 = linear) |
//! | 17 | 1 | `u8` | Anchor count |
//! | 18 | 2 | — | Reserved (zeros) |
//! | 20 | 4 | `f32 LE` | Display hint min luminance nits |
//! | 24 | 4 | `f32 LE` | Display hint max luminance nits |
//! | 28 | N×8 | N×(f32, f32) | Anchor pairs (input, output) |

use crate::{
    ChromaSubsampling, ChromaticityPoint, ContentLightLevel, CurveAnchor, CurveType, DisplayHint,
    DynamicMeta, MasteringDisplay, MetaError, Precision, StaticMeta, Tier, ToneMapCurve,
    compatibility::QDRV_FORMAT_VERSION,
};

const STATIC_MAGIC_V1: &[u8; 8] = b"QDRVMETA";
const STATIC_MAGIC_V2: &[u8; 8] = b"QDM2META";
const DYNAMIC_MAGIC_V2: &[u8; 8] = b"QDD2META";
const STATIC_SIZE: usize = 128;
const MAX_TONE_MAP_ANCHORS: usize = 32;
const V2_BINARY_HEADER_LEN: usize = 14; // magic(8) + schema_version(2) + json_len(4)

fn binary_meta_error(message: &'static str) -> MetaError {
    MetaError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

fn read_f64_at(data: &[u8], off: usize) -> Result<f64, MetaError> {
    let end = off
        .checked_add(8)
        .ok_or_else(|| binary_meta_error("binary metadata offset overflow"))?;
    let bytes = data
        .get(off..end)
        .ok_or_else(|| binary_meta_error("binary metadata truncated while reading f64"))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    Ok(f64::from_le_bytes(raw))
}

fn read_f32_at(data: &[u8], off: usize) -> Result<f32, MetaError> {
    let end = off
        .checked_add(4)
        .ok_or_else(|| binary_meta_error("binary metadata offset overflow"))?;
    let bytes = data
        .get(off..end)
        .ok_or_else(|| binary_meta_error("binary metadata truncated while reading f32"))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(bytes);
    Ok(f32::from_le_bytes(raw))
}

fn read_u64_at(data: &[u8], off: usize) -> Result<u64, MetaError> {
    let end = off
        .checked_add(8)
        .ok_or_else(|| binary_meta_error("binary metadata offset overflow"))?;
    let bytes = data
        .get(off..end)
        .ok_or_else(|| binary_meta_error("binary metadata truncated while reading u64"))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(raw))
}

/// Encodes a [`StaticMeta`] to a length-prefixed v2 binary representation.
///
/// # Errors
/// Returns [`MetaError::Json`] if `meta` cannot be serialised, or
/// [`MetaError::Io`] (kind `InvalidInput`) if the resulting JSON payload
/// exceeds `u32::MAX` bytes (e.g., pathologically long `compatibility_tags`).
pub fn encode_static_binary(meta: &StaticMeta) -> Result<Vec<u8>, MetaError> {
    let json = serde_json::to_vec(meta)?;
    let len_u32 = u32::try_from(json.len()).map_err(|_| {
        MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "static metadata JSON length exceeds u32::MAX",
        ))
    })?;
    let mut out = Vec::with_capacity(V2_BINARY_HEADER_LEN + json.len());
    out.extend_from_slice(STATIC_MAGIC_V2);
    out.extend_from_slice(&meta.metadata_schema_version.to_le_bytes());
    out.extend_from_slice(&len_u32.to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Decodes a [`StaticMeta`] from its binary representation.
///
/// Two on-disk formats are recognised:
///
/// - **v2 (current)** — variable-length, JSON-wrapped: 8-byte magic
///   (`QDM2META`), 2-byte LE schema version, 4-byte LE JSON length, and
///   the JSON payload. Tried first by the decoder.
/// - **v1 (legacy)** — fixed 128-byte binary layout with magic `QDRVMETA`.
///   Used as a fallback when the v2 magic does not match. Documented in
///   the module-level layout table above.
pub fn decode_static_binary(data: &[u8]) -> Result<StaticMeta, MetaError> {
    if data.len() >= V2_BINARY_HEADER_LEN && &data[0..8] == STATIC_MAGIC_V2 {
        let schema_version = u16::from_le_bytes([data[8], data[9]]);
        let json_len = u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;
        let end = V2_BINARY_HEADER_LEN
            .checked_add(json_len)
            .ok_or_else(|| binary_meta_error("v2 static metadata length overflow"))?;
        if data.len() > end {
            return Err(binary_meta_error(
                "v2 static metadata has trailing bytes after the declared payload",
            ));
        }
        let payload = data
            .get(V2_BINARY_HEADER_LEN..end)
            .ok_or_else(|| binary_meta_error("truncated v2 static metadata payload"))?;
        let json = std::str::from_utf8(payload)
            .map_err(|e| MetaError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        let mut meta = crate::from_json_strict_static(json)?;
        meta.metadata_schema_version = schema_version;
        meta.validate()
            .map_err(|e| MetaError::Validation(e.to_string()))?;
        return Ok(meta);
    }

    decode_static_binary_v1(data)
}

fn decode_static_binary_v1(data: &[u8]) -> Result<StaticMeta, MetaError> {
    if data.len() < STATIC_SIZE || &data[0..8] != STATIC_MAGIC_V1 {
        return Err(MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid binary static metadata header",
        )));
    }
    if data.len() > STATIC_SIZE {
        return Err(binary_meta_error(
            "v1 static metadata has trailing bytes after the fixed-size record",
        ));
    }

    let tier = match data[8] {
        0 => Tier::Mastering,
        1 => Tier::Delivery,
        _ => {
            return Err(MetaError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid tier byte",
            )));
        }
    };
    let precision = match data[9] {
        0 => Precision::Float64,
        1 => Precision::Float32,
        _ => {
            return Err(MetaError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid precision byte",
            )));
        }
    };
    let chroma = match data[10] {
        0 => ChromaSubsampling::Yuv444,
        1 => ChromaSubsampling::Yuv420,
        _ => {
            return Err(MetaError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid chroma byte",
            )));
        }
    };

    let tf = match tier {
        Tier::Delivery => "st2084_pq",
        Tier::Mastering => "linear",
    };

    Ok(StaticMeta {
        qdrv_version: QDRV_FORMAT_VERSION.to_string(),
        metadata_schema_version: 1,
        tier,
        precision,
        colour_standard: "rec2100".to_string(),
        colour_primaries: "rec2020".to_string(),
        transfer_function: tf.to_string(),
        dynamic_metadata_standard: "st2094".to_string(),
        chroma_subsampling: chroma,
        mastering_display: MasteringDisplay {
            red_primary: ChromaticityPoint {
                x: read_f64_at(data, 12)?,
                y: read_f64_at(data, 20)?,
            },
            green_primary: ChromaticityPoint {
                x: read_f64_at(data, 28)?,
                y: read_f64_at(data, 36)?,
            },
            blue_primary: ChromaticityPoint {
                x: read_f64_at(data, 44)?,
                y: read_f64_at(data, 52)?,
            },
            white_point: ChromaticityPoint {
                x: read_f64_at(data, 60)?,
                y: read_f64_at(data, 68)?,
            },
            min_luminance_nits: read_f64_at(data, 76)?,
            max_luminance_nits: read_f64_at(data, 84)?,
        },
        content_light_level: ContentLightLevel {
            max_cll_nits: read_f32_at(data, 92)?,
            max_fall_nits: read_f32_at(data, 96)?,
        },
        compatibility_tags: vec!["legacy_binary_v1_decode".to_string()],
    })
}

/// Encodes a [`DynamicMeta`] to a compact binary representation.
///
/// Returns an error if the tone curve carries more anchors than the binary
/// format can represent.
pub fn encode_dynamic_binary(meta: &DynamicMeta) -> Result<Vec<u8>, MetaError> {
    if meta.tone_map_curve.anchors.len() > MAX_TONE_MAP_ANCHORS {
        return Err(MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "dynamic metadata has {} anchors; maximum supported is {MAX_TONE_MAP_ANCHORS}",
                meta.tone_map_curve.anchors.len()
            ),
        )));
    }
    let json = serde_json::to_vec(meta)?;
    let len_u32 = u32::try_from(json.len()).map_err(|_| {
        MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "dynamic metadata JSON length exceeds u32::MAX",
        ))
    })?;
    let mut out = Vec::with_capacity(V2_BINARY_HEADER_LEN + json.len());
    out.extend_from_slice(DYNAMIC_MAGIC_V2);
    out.extend_from_slice(&meta.metadata_schema_version.to_le_bytes());
    out.extend_from_slice(&len_u32.to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Decodes a [`DynamicMeta`] from its compact binary representation.
pub fn decode_dynamic_binary(data: &[u8]) -> Result<DynamicMeta, MetaError> {
    if data.len() >= V2_BINARY_HEADER_LEN && &data[0..8] == DYNAMIC_MAGIC_V2 {
        let schema_version = u16::from_le_bytes([data[8], data[9]]);
        let json_len = u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;
        let end = V2_BINARY_HEADER_LEN
            .checked_add(json_len)
            .ok_or_else(|| binary_meta_error("v2 dynamic metadata length overflow"))?;
        if data.len() > end {
            return Err(binary_meta_error(
                "v2 dynamic metadata has trailing bytes after the declared payload",
            ));
        }
        let payload = data
            .get(V2_BINARY_HEADER_LEN..end)
            .ok_or_else(|| binary_meta_error("truncated v2 dynamic metadata payload"))?;
        let json = std::str::from_utf8(payload)
            .map_err(|e| MetaError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        let mut meta = crate::from_json_strict_dynamic(json)?;
        meta.metadata_schema_version = schema_version;
        meta.validate()
            .map_err(|e| MetaError::Validation(e.to_string()))?;
        return Ok(meta);
    }

    decode_dynamic_binary_v1(data)
}

// V1 dynamic metadata has no magic byte (the layout assigns offset 0 to
// `frame_index` for backward compatibility with the original format). Because
// the decoder cannot recognise the format up front, every parsed payload is
// re-validated with `DynamicMeta::validate()` before being returned, so
// non-QDRV byte streams that happen to fit the v1 size envelope are rejected
// rather than producing nonsense per-field values.
fn decode_dynamic_binary_v1(data: &[u8]) -> Result<DynamicMeta, MetaError> {
    if data.len() < 28 {
        return Err(MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "binary dynamic metadata too short",
        )));
    }

    let frame_index = read_u64_at(data, 0)?;
    let curve_type = match data[16] {
        0 => CurveType::Bezier,
        1 => CurveType::Linear,
        _ => {
            return Err(MetaError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid curve type byte",
            )));
        }
    };
    let anchor_count = data[17] as usize;
    if anchor_count > MAX_TONE_MAP_ANCHORS {
        return Err(MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("anchor count {anchor_count} exceeds maximum {MAX_TONE_MAP_ANCHORS}"),
        )));
    }
    let anchor_bytes = anchor_count
        .checked_mul(8)
        .ok_or_else(|| binary_meta_error("anchor byte count overflow"))?;
    let required_len = 28usize
        .checked_add(anchor_bytes)
        .ok_or_else(|| binary_meta_error("dynamic metadata length overflow"))?;

    if data.len() < required_len {
        return Err(MetaError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "truncated anchor data",
        )));
    }
    if data.len() > required_len {
        return Err(binary_meta_error(
            "v1 dynamic metadata has trailing bytes after the declared anchors",
        ));
    }

    // F-3: anchor_count is bounded ≤ MAX_TONE_MAP_ANCHORS above (32), so
    // `Vec::with_capacity` is already safe here, but try_reserve_exact
    // keeps the codebase's untrusted-input allocation pattern uniform.
    let mut anchors: Vec<CurveAnchor> = Vec::new();
    anchors
        .try_reserve_exact(anchor_count)
        .map_err(|_| binary_meta_error("anchor buffer allocation failed"))?;
    let mut o = 28;
    for _ in 0..anchor_count {
        anchors.push(CurveAnchor {
            input: read_f32_at(data, o)?,
            output: read_f32_at(data, o + 4)?,
        });
        o += 8;
    }

    let meta = DynamicMeta {
        metadata_schema_version: 1,
        frame_index,
        scene_peak_luminance_nits: read_f32_at(data, 8)?,
        scene_average_luminance_nits: read_f32_at(data, 12)?,
        tone_map_curve: ToneMapCurve {
            curve_type,
            anchors,
        },
        target_display_hint: DisplayHint {
            min_luminance_nits: read_f32_at(data, 20)?,
            max_luminance_nits: read_f32_at(data, 24)?,
        },
        open_dynamic_v2: None,
        inverse_tone_mapping_hint: None,
        creator_intent_locked: false,
    };
    meta.validate()
        .map_err(|e| MetaError::Validation(e.to_string()))?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms v2 static metadata encodes with header + JSON and round-trips.
    #[test]
    fn test_static_binary_roundtrip() {
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let encoded = encode_static_binary(&meta).unwrap();
        assert!(encoded.len() > V2_BINARY_HEADER_LEN);
        assert_eq!(&encoded[0..8], STATIC_MAGIC_V2);
        let decoded = decode_static_binary(&encoded).unwrap();
        assert_eq!(meta, decoded);
    }

    /// Verifies the mastering-tier defaults survive a static binary encode/decode cycle without corruption.
    #[test]
    fn test_static_binary_mastering_roundtrip() {
        let meta = StaticMeta::default_mastering();
        let encoded = encode_static_binary(&meta).unwrap();
        let decoded = decode_static_binary(&encoded).unwrap();
        assert_eq!(meta, decoded);
    }

    /// Confirms v2 dynamic metadata encodes with header + JSON and round-trips.
    #[test]
    fn test_dynamic_binary_roundtrip() {
        let meta = DynamicMeta::new(42, 1200.0, 180.0);
        let encoded = encode_dynamic_binary(&meta).unwrap();
        assert!(encoded.len() > V2_BINARY_HEADER_LEN);
        assert_eq!(&encoded[0..8], DYNAMIC_MAGIC_V2);
        let decoded = decode_dynamic_binary(&encoded).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn test_dynamic_binary_rejects_anchor_overflow() {
        let mut meta = DynamicMeta::new(0, 1000.0, 200.0);
        meta.tone_map_curve.anchors = (0..33)
            .map(|i| {
                let x = i as f32 / 32.0;
                CurveAnchor {
                    input: x,
                    output: x,
                }
            })
            .collect();

        let err = encode_dynamic_binary(&meta).unwrap_err();
        assert!(format!("{err}").contains("maximum supported is"));
    }

    #[test]
    fn test_dynamic_binary_decode_rejects_anchor_overflow() {
        let mut data = vec![0u8; 28 + 33 * 8];
        data[0..8].copy_from_slice(STATIC_MAGIC_V1);
        data[16] = 0;
        data[17] = 33;
        let err = decode_dynamic_binary(&data).unwrap_err();
        assert!(format!("{err}").contains("exceeds maximum"));
    }

    /// Ensures both decoders reject inputs shorter than their respective minimum header sizes (static and dynamic paths).
    #[test]
    fn test_binary_rejects_truncated() {
        assert!(decode_static_binary(&[0u8; 10]).is_err());
        assert!(decode_dynamic_binary(&[0u8; 5]).is_err());
    }

    /// LOW-1 regression: decoders reject extra bytes after the declared payload,
    /// so "accepted binary metadata" cannot be broader than the declared length.
    #[test]
    fn test_static_binary_rejects_trailing_bytes() {
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let mut encoded = encode_static_binary(&meta).unwrap();
        encoded.push(0);
        assert!(decode_static_binary(&encoded).is_err());
    }

    #[test]
    fn test_dynamic_binary_rejects_trailing_bytes() {
        let meta = DynamicMeta::new(42, 1200.0, 180.0);
        let mut encoded = encode_dynamic_binary(&meta).unwrap();
        encoded.push(0);
        assert!(decode_dynamic_binary(&encoded).is_err());
    }

    #[test]
    fn test_v2_binary_encoding_is_deterministic() {
        let mut meta = DynamicMeta::new(7, 1500.0, 300.0);
        meta.creator_intent_locked = true;
        let a = encode_dynamic_binary(&meta).unwrap();
        let b = encode_dynamic_binary(&meta).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_decode_static_binary_rejects_invalid_v2_schema_header() {
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let mut encoded = encode_static_binary(&meta).unwrap();
        encoded[8..10].copy_from_slice(&999u16.to_le_bytes());
        let err = decode_static_binary(&encoded).unwrap_err();
        assert!(format!("{err}").contains("metadata_schema_version"));
    }

    #[test]
    fn test_decode_dynamic_binary_rejects_invalid_v2_schema_header() {
        let meta = DynamicMeta::new(1, 1000.0, 250.0);
        let mut encoded = encode_dynamic_binary(&meta).unwrap();
        encoded[8..10].copy_from_slice(&999u16.to_le_bytes());
        let err = decode_dynamic_binary(&encoded).unwrap_err();
        assert!(format!("{err}").contains("metadata_schema_version"));
    }
}
