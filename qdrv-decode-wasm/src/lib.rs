// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-decode-wasm
//!
//! WebAssembly bindings for QDRV delivery-tier tone mapping in the browser
//! (roadmap item 2: in-browser playback of `.qdrv32`).
//!
//! The native AV1 and mastering codecs (`rav1e`, `dav1d`, `fpzip`, `zfp`) do
//! not build for `wasm32`, so the browser pipeline decodes the AV1 bitstream
//! with the platform [WebCodecs] `VideoDecoder` and hands the decoded
//! delivery-tier pixels to this crate for QDRV metadata-driven tone mapping.
//! This crate is therefore intentionally **codec-free**: it depends on the
//! pure-Rust `qdrv-core`, `qdrv-meta`, `qdrv-decode`, `qdrv-mux` AVIF wrapping,
//! and metadata-OBU parsing portions of `qdrv-codec`, which all compile to
//! `wasm32-unknown-unknown`.
//!
//! The tone-mapping logic itself lives in [`tone_map_frame_core`], which is
//! target-independent and unit-tested on the host. The `wasm-bindgen` export
//! is a thin wrapper compiled only for `wasm32`, so the native workspace gates
//! build and test the same logic without pulling the wasm toolchain.
//!
//! [WebCodecs]: https://developer.mozilla.org/docs/Web/API/WebCodecs_API

use qdrv_codec::extract_all_qdrv_metadata;
use qdrv_core::colors::ncl;
use qdrv_core::pixel::Pixel32;
use qdrv_decode::{TargetDisplay, tone_map_frame_with_objects};
use qdrv_meta::{
    DynamicMeta, ObjectMeta, StaticMeta,
    compatibility::{
        CompatibilityPolicy, METADATA_SCHEMA_V1, METADATA_SCHEMA_V2, validate_compatibility,
    },
};
use qdrv_mux::{AvifConfig, write_avif};
use serde::Serialize;

const QDRV_MAGIC: &[u8; 4] = b"QDRV";
const QDRV_HEADER_SIZE: usize = 28;
const CONTAINER_VERSION_V1: u16 = 1;
const CONTAINER_VERSION_V2: u16 = 2;
const TIER_DELIVERY: u8 = 1;
const CODEC_AV1: u8 = 1;
const MAX_JSON_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_FRAME_PIXELS: usize = 16 * 1024 * 1024;
const MAX_FRAME_PAYLOAD_BYTES: usize = 512 * 1024 * 1024;
const MAX_FRAME_COUNT: usize = 100_000;
const MIN_COMPRESSED_FRAME_BUDGET: usize = 256 * 1024;

#[derive(Serialize)]
struct ParsedQdrv32Container {
    version: u16,
    width: u32,
    height: u32,
    frame_count: u32,
    static_metadata: StaticMeta,
    frames: Vec<ParsedQdrv32Frame>,
}

#[derive(Serialize)]
struct ParsedQdrv32Frame {
    frame_index: u64,
    dynamic_metadata: DynamicMeta,
    payload_offset: usize,
    payload_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct Qdrv32Header {
    version: u16,
    tier: u8,
    codec: u8,
    width: u32,
    height: u32,
    frame_count: u32,
    static_meta_len: u32,
}

/// Tone-maps one delivery-tier frame of PQ-encoded Float32 RGB pixels using
/// QDRV per-frame metadata, returning tone-mapped PQ RGB ready for a
/// display encode on the JavaScript side.
///
/// # Arguments
/// * `pq_rgb` — interleaved `R, G, B` per pixel, length `width * height * 3`.
/// * `width`, `height` — frame dimensions in pixels.
/// * `dynamic_json` — the per-frame [`DynamicMeta`] as JSON.
/// * `object_json` — optional per-frame [`ObjectMeta`] as JSON (flat object
///   regions and/or 360°/immersive spherical regions). `None` for ordinary
///   global tone mapping.
/// * `target_max_nits`, `target_min_nits` — capabilities of the display the
///   browser is rendering to.
///
/// # Returns
/// Interleaved tone-mapped `R, G, B` in the same layout and length as the
/// input.
///
/// # Errors
/// Returns a human-readable message if the pixel buffer length does not match
/// the dimensions, if either metadata document fails to parse or validate, or
/// if the supplied `object_json` frame index does not match the dynamic
/// metadata frame index.
pub fn tone_map_frame_core(
    pq_rgb: &[f32],
    width: u32,
    height: u32,
    dynamic_json: &str,
    object_json: Option<&str>,
    target_max_nits: f32,
    target_min_nits: f32,
) -> Result<Vec<f32>, String> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .ok_or("frame dimensions overflow usize")?;
    if pq_rgb.len() != expected {
        return Err(format!(
            "pixel buffer length {} does not match width*height*3 = {expected}",
            pq_rgb.len()
        ));
    }

    let dynamic: DynamicMeta =
        qdrv_meta::from_json(dynamic_json).map_err(|e| format!("dynamic metadata: {e}"))?;
    dynamic
        .validate()
        .map_err(|e| format!("dynamic metadata invalid: {e}"))?;

    let object_meta = match object_json {
        Some(json) => {
            let om: ObjectMeta =
                qdrv_meta::from_json(json).map_err(|e| format!("object metadata: {e}"))?;
            om.validate()
                .map_err(|e| format!("object metadata invalid: {e}"))?;
            Some(om)
        }
        None => None,
    };

    let pixels: Vec<Pixel32> = pq_rgb
        .chunks_exact(3)
        .map(|c| Pixel32::new_unchecked(c[0], c[1], c[2]))
        .collect();

    let target = TargetDisplay {
        min_nits: target_min_nits,
        max_nits: target_max_nits,
    };

    let mapped = tone_map_frame_with_objects(
        &pixels,
        width,
        height,
        &dynamic,
        object_meta.as_ref(),
        &target,
    )
    .map_err(|e| format!("tone mapping failed: {e}"))?;

    let mut out = Vec::with_capacity(mapped.len() * 3);
    for p in &mapped {
        out.push(p.r);
        out.push(p.g);
        out.push(p.b);
    }
    Ok(out)
}

/// Converts BT.2020 non-constant-luminance Y'CbCr planes to the interleaved
/// PQ R'G'B' layout expected by [`tone_map_frame_core`].
///
/// This bridges a WebCodecs `VideoFrame` — the browser decodes the AV1 payload
/// to 12-bit 4:4:4 Y'CbCr — to the QDRV tone mapper. The JavaScript side
/// de-quantises the decoder's code values to the normalised inputs expected
/// here:
///
/// * `y` — luma normalised to `[0.0, 1.0]`.
/// * `cb`, `cr` — chroma normalised to `[-0.5, 0.5]`.
///
/// All three planes are full-resolution 4:4:4 (`width * height` samples each),
/// matching QDRV's 4:4:4 delivery encoding. The transfer is unchanged: the AV1
/// signal is already SMPTE ST 2084 PQ-encoded, so the R'G'B' output is PQ.
///
/// # Errors
/// Returns a message if any plane length does not equal `width * height`.
pub fn yuv_ncl_to_pq_rgb_core(
    y: &[f32],
    cb: &[f32],
    cr: &[f32],
    width: u32,
    height: u32,
) -> Result<Vec<f32>, String> {
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or("frame dimensions overflow usize")?;
    if y.len() != count || cb.len() != count || cr.len() != count {
        return Err(format!(
            "plane lengths (y={}, cb={}, cr={}) must each equal width*height = {count}",
            y.len(),
            cb.len(),
            cr.len()
        ));
    }

    // ITU-R Rec. 2100 non-constant-luminance inverse matrix, sourced from the
    // shared luma coefficients in `qdrv-core` so the wasm path matches the
    // native pipeline.
    let kr = ncl::KR as f32;
    let kg = ncl::KG as f32;
    let kb = ncl::KB as f32;
    let cr_to_r = 2.0 * (1.0 - kr);
    let cb_to_b = 2.0 * (1.0 - kb);

    let mut out = Vec::with_capacity(count * 3);
    for ((&yv, &cbv), &crv) in y.iter().zip(cb.iter()).zip(cr.iter()) {
        let r = yv + cr_to_r * crv;
        let b = yv + cb_to_b * cbv;
        let g = (yv - kr * r - kb * b) / kg;
        out.push(r);
        out.push(g);
        out.push(b);
    }
    Ok(out)
}

/// Samples the per-frame global tone-map curve into a 1-D lookup table.
///
/// The browser WebGPU compute shader does the per-pixel PQ transfer and
/// luminance scaling on the GPU, but evaluates the (monotone-cubic or linear)
/// tone curve through this CPU-built LUT so the GPU result matches the native
/// curve evaluation exactly. `size` is the number of evenly spaced samples in
/// `[0, 1]` (must be at least 2).
///
/// # Errors
/// Returns a message if `size < 2`, or if the dynamic metadata fails to parse
/// or validate.
pub fn build_tone_curve_lut_core(dynamic_json: &str, size: u32) -> Result<Vec<f32>, String> {
    if size < 2 {
        return Err("tone-curve LUT size must be at least 2".to_string());
    }
    let dynamic: DynamicMeta =
        qdrv_meta::from_json(dynamic_json).map_err(|e| format!("dynamic metadata: {e}"))?;
    dynamic
        .validate()
        .map_err(|e| format!("dynamic metadata invalid: {e}"))?;
    let curve = &dynamic.tone_map_curve;
    let denom = (size - 1) as f32;
    let lut = (0..size)
        .map(|i| curve.evaluate(i as f32 / denom))
        .collect();
    Ok(lut)
}

/// Extracts every in-bitstream QDRV per-frame dynamic-metadata payload from an
/// AV1 stream and returns the decoded [`DynamicMeta`] entries as a JSON array.
///
/// This is the read side of the browser pipeline: `qdrv mux` embeds each
/// frame's dynamic metadata as an ITU-T T.35 metadata OBU, so the same AV1
/// bytes the WebCodecs decoder consumes also carry the metadata. The browser
/// passes the demuxed stream bytes here, gets one metadata document per frame,
/// and feeds each to [`tone_map_frame_core`]. Only the pure-Rust OBU parser
/// from `qdrv-codec` is used (its native codecs are off), so this links on
/// `wasm32`.
///
/// # Errors
/// Returns a message if OBU parsing fails or any payload is not valid QDRV
/// binary dynamic metadata.
pub fn extract_stream_metadata_core(stream: &[u8]) -> Result<String, String> {
    let payloads = extract_all_qdrv_metadata(stream).map_err(|e| format!("metadata OBU: {e}"))?;
    let metas: Vec<DynamicMeta> = payloads
        .iter()
        .map(|p| {
            qdrv_meta::binary::decode_dynamic_binary(p)
                .map_err(|e| format!("decode dynamic metadata: {e}"))
        })
        .collect::<Result<_, _>>()?;
    qdrv_meta::to_json(&metas).map_err(|e| format!("serialise: {e}"))
}

/// Parses a delivery-tier `.qdrv32` container for browser playback and returns
/// a JSON manifest with validated static metadata, per-frame dynamic metadata,
/// and byte offsets to each AV1 payload inside the original input buffer.
///
/// The browser keeps ownership of the original `ArrayBuffer` and slices the
/// returned payload ranges for WebCodecs. That avoids copying binary AV1 data
/// into JSON while still putting all QDRV container validation in the wasm core.
///
/// # Errors
/// Returns a message if the file is truncated, not a supported delivery-tier
/// AV1 QDRV container, declares oversized blocks, carries invalid metadata, or
/// has trailing bytes after the declared frame blocks.
pub fn parse_qdrv32_container_core(data: &[u8]) -> Result<String, String> {
    let header = parse_qdrv32_header(data)?;
    validate_qdrv32_header(header)?;

    let expected_pixels = (header.width as usize)
        .checked_mul(header.height as usize)
        .ok_or("frame dimensions overflow usize")?;
    if expected_pixels > MAX_FRAME_PIXELS {
        return Err(format!(
            "frame area {} exceeds limit {MAX_FRAME_PIXELS}",
            expected_pixels
        ));
    }

    let mut offset = QDRV_HEADER_SIZE;
    let static_meta_bytes = take_range(
        data,
        &mut offset,
        header.static_meta_len as usize,
        Some(MAX_JSON_BLOCK_BYTES),
        "static metadata JSON block",
    )?;
    let static_meta: StaticMeta = parse_json_block(static_meta_bytes, "static metadata")?;
    static_meta
        .validate()
        .map_err(|e| format!("static metadata invalid: {e}"))?;
    ensure_metadata_schema_supported_for_container(
        header.version,
        static_meta.metadata_schema_version,
    )?;

    let frame_count = header.frame_count as usize;
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(frame_count)
        .map_err(|_| "could not reserve frame manifest entries".to_string())?;
    for frame_idx in 0..frame_count {
        let dyn_len = read_u32_at(data, offset, "dynamic metadata length")? as usize;
        offset = offset
            .checked_add(4)
            .ok_or("dynamic metadata length offset overflow")?;
        let dynamic_bytes = take_range(
            data,
            &mut offset,
            dyn_len,
            Some(MAX_JSON_BLOCK_BYTES),
            "dynamic metadata JSON block",
        )?;
        let dynamic: DynamicMeta = parse_json_block(dynamic_bytes, "dynamic metadata")?;
        dynamic
            .validate()
            .map_err(|e| format!("frame {frame_idx}: dynamic metadata invalid: {e}"))?;
        ensure_metadata_schema_supported_for_container(
            header.version,
            dynamic.metadata_schema_version,
        )?;
        validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .map_err(|e| format!("frame {frame_idx}: metadata compatibility: {e}"))?;
        if dynamic.frame_index != frame_idx as u64 {
            return Err(format!(
                "frame {frame_idx}: dynamic metadata frame_index {} does not match stream position",
                dynamic.frame_index
            ));
        }

        let payload_len = read_u32_at(data, offset, "AV1 payload length")? as usize;
        offset = offset
            .checked_add(4)
            .ok_or("AV1 payload length offset overflow")?;
        let max_payload = compressed_frame_budget(expected_pixels)?;
        if payload_len > max_payload {
            return Err(format!(
                "frame {frame_idx}: AV1 payload length {payload_len} exceeds limit {max_payload}"
            ));
        }
        let payload_offset = offset;
        // `payload_len` is already bounded against `compressed_frame_budget`
        // above, so no additional cap is needed here.
        take_range(data, &mut offset, payload_len, None, "AV1 payload")?;
        frames.push(ParsedQdrv32Frame {
            frame_index: dynamic.frame_index,
            dynamic_metadata: dynamic,
            payload_offset,
            payload_len,
        });
    }

    if offset != data.len() {
        return Err(format!(
            "QDRV container has {} trailing byte(s) after declared frames",
            data.len() - offset
        ));
    }

    let parsed = ParsedQdrv32Container {
        version: header.version,
        width: header.width,
        height: header.height,
        frame_count: header.frame_count,
        static_metadata: static_meta,
        frames,
    };
    qdrv_meta::to_json(&parsed).map_err(|e| format!("serialise QDRV manifest: {e}"))
}

/// Wraps one QDRV AV1 still-picture payload as a single-image AVIF file.
///
/// Direct `.qdrv32` delivery frames are stored as independent AV1 still-picture
/// bitstreams. Browser `VideoDecoder` implementations are video-oriented and
/// can reject those still-picture temporal units at the keyframe gate, so the
/// web runtime feeds them to `ImageDecoder` as AVIF instead.
///
/// # Errors
/// Returns a message if the dimensions are zero, the AV1 payload is empty, or
/// the AVIF container writer detects an overflow.
pub fn wrap_av1_still_as_avif_core(
    av1_data: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let config = AvifConfig::new(width, height);
    write_avif(&mut out, &config, av1_data, None)
        .map_err(|e| format!("wrap AV1 still picture as AVIF: {e}"))?;
    Ok(out)
}

fn parse_qdrv32_header(data: &[u8]) -> Result<Qdrv32Header, String> {
    if data.len() < QDRV_HEADER_SIZE {
        return Err("truncated QDRV header".to_string());
    }
    if data.get(0..4) != Some(QDRV_MAGIC.as_slice()) {
        return Err("invalid QDRV magic".to_string());
    }
    let flags = read_u32_at(data, 20, "reserved flags")?;
    if flags != 0 {
        return Err(format!("reserved QDRV flags must be zero, got {flags}"));
    }
    Ok(Qdrv32Header {
        version: read_u16_at(data, 4, "container version")?,
        tier: *data.get(6).ok_or("truncated tier byte")?,
        codec: *data.get(7).ok_or("truncated codec byte")?,
        width: read_u32_at(data, 8, "frame width")?,
        height: read_u32_at(data, 12, "frame height")?,
        frame_count: read_u32_at(data, 16, "frame count")?,
        static_meta_len: read_u32_at(data, 24, "static metadata length")?,
    })
}

fn validate_qdrv32_header(header: Qdrv32Header) -> Result<(), String> {
    if !matches!(header.version, CONTAINER_VERSION_V1 | CONTAINER_VERSION_V2) {
        return Err(format!(
            "unsupported QDRV container version {}",
            header.version
        ));
    }
    if header.tier != TIER_DELIVERY {
        return Err(format!(
            "browser playback accepts delivery-tier .qdrv32 only, got tier {}",
            header.tier
        ));
    }
    if header.codec != CODEC_AV1 {
        return Err(format!(
            "browser playback accepts AV1-compressed .qdrv32 only, got codec {}",
            header.codec
        ));
    }
    if header.width == 0 || header.height == 0 {
        return Err(format!(
            "invalid frame dimensions {}x{}",
            header.width, header.height
        ));
    }
    if header.frame_count as usize > MAX_FRAME_COUNT {
        return Err(format!(
            "frame count {} exceeds limit {MAX_FRAME_COUNT}",
            header.frame_count
        ));
    }
    Ok(())
}

fn read_u16_at(data: &[u8], offset: usize, context: &'static str) -> Result<u16, String> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| format!("{context} offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| format!("truncated {context}"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_at(data: &[u8], offset: usize, context: &'static str) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| format!("{context} offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| format!("truncated {context}"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn take_range<'a>(
    data: &'a [u8],
    offset: &mut usize,
    len: usize,
    limit: Option<usize>,
    context: &'static str,
) -> Result<&'a [u8], String> {
    // The caller states its cap explicitly. `None` is reserved for lengths the
    // caller has already bounded (the AV1 payload path checks its length
    // against `compressed_frame_budget` before calling here), so a new call
    // site cannot silently opt out of bounding by its choice of context label.
    if let Some(limit) = limit
        && len > limit
    {
        return Err(format!("{context} length {len} exceeds limit {limit}"));
    }
    let end = offset
        .checked_add(len)
        .ok_or_else(|| format!("{context} range overflow"))?;
    let range = data
        .get(*offset..end)
        .ok_or_else(|| format!("truncated {context}"))?;
    *offset = end;
    Ok(range)
}

fn parse_json_block<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    context: &'static str,
) -> Result<T, String> {
    let json = std::str::from_utf8(bytes).map_err(|e| format!("{context}: invalid UTF-8: {e}"))?;
    qdrv_meta::from_json(json).map_err(|e| format!("{context}: {e}"))
}

fn ensure_metadata_schema_supported_for_container(
    container_version: u16,
    metadata_schema_version: u16,
) -> Result<(), String> {
    match container_version {
        CONTAINER_VERSION_V1 => {
            if metadata_schema_version != METADATA_SCHEMA_V1 {
                return Err(format!(
                    "container version {CONTAINER_VERSION_V1} requires metadata schema version \
                     {METADATA_SCHEMA_V1}, got {metadata_schema_version}"
                ));
            }
            Ok(())
        }
        CONTAINER_VERSION_V2 => {
            if metadata_schema_version == METADATA_SCHEMA_V1
                || metadata_schema_version == METADATA_SCHEMA_V2
            {
                Ok(())
            } else {
                Err(format!(
                    "container version {CONTAINER_VERSION_V2} does not support metadata schema \
                     version {metadata_schema_version}"
                ))
            }
        }
        _ => Err(format!(
            "unsupported QDRV container version {container_version}"
        )),
    }
}

fn compressed_frame_budget(expected_pixels: usize) -> Result<usize, String> {
    let uncompressed = expected_pixels
        .checked_mul(3)
        .and_then(|n| n.checked_mul(4))
        .ok_or("uncompressed frame byte size overflow")?;
    let expanded = uncompressed
        .checked_mul(8)
        .ok_or("compressed frame budget overflow")?;
    Ok(expanded.clamp(MIN_COMPRESSED_FRAME_BUDGET, MAX_FRAME_PAYLOAD_BYTES))
}

// Thin browser entry points, compiled only for the wasm32 target so the native
// workspace gates never pull the wasm toolchain.
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Browser entry point for [`tone_map_frame_core`].
///
/// `pq_rgb` arrives as a `Float32Array`; the result is returned as a
/// `Float32Array`, or the call throws with the error message.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn tone_map_frame(
    pq_rgb: &[f32],
    width: u32,
    height: u32,
    dynamic_json: &str,
    object_json: Option<String>,
    target_max_nits: f32,
    target_min_nits: f32,
) -> Result<Vec<f32>, JsError> {
    tone_map_frame_core(
        pq_rgb,
        width,
        height,
        dynamic_json,
        object_json.as_deref(),
        target_max_nits,
        target_min_nits,
    )
    .map_err(|e| JsError::new(&e))
}

/// Browser entry point for [`yuv_ncl_to_pq_rgb_core`].
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn yuv_ncl_to_pq_rgb(
    y: &[f32],
    cb: &[f32],
    cr: &[f32],
    width: u32,
    height: u32,
) -> Result<Vec<f32>, JsError> {
    yuv_ncl_to_pq_rgb_core(y, cb, cr, width, height).map_err(|e| JsError::new(&e))
}

/// Browser entry point for [`build_tone_curve_lut_core`].
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn build_tone_curve_lut(dynamic_json: &str, size: u32) -> Result<Vec<f32>, JsError> {
    build_tone_curve_lut_core(dynamic_json, size).map_err(|e| JsError::new(&e))
}

/// Browser entry point for [`extract_stream_metadata_core`].
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn extract_stream_metadata(stream: &[u8]) -> Result<String, JsError> {
    extract_stream_metadata_core(stream).map_err(|e| JsError::new(&e))
}

/// Browser entry point for [`parse_qdrv32_container_core`].
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn parse_qdrv32_container(data: &[u8]) -> Result<String, JsError> {
    parse_qdrv32_container_core(data).map_err(|e| JsError::new(&e))
}

/// Browser entry point for [`wrap_av1_still_as_avif_core`].
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn wrap_av1_still_as_avif(
    av1_data: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, JsError> {
    wrap_av1_still_as_avif_core(av1_data, width, height).map_err(|e| JsError::new(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct ManifestForTest {
        width: u32,
        height: u32,
        frame_count: u32,
        frames: Vec<FrameForTest>,
    }

    #[derive(Deserialize)]
    struct FrameForTest {
        frame_index: u64,
        dynamic_metadata: qdrv_meta::DynamicMeta,
        payload_offset: usize,
        payload_len: usize,
    }

    fn sample_dynamic_json() -> String {
        let dynamic = qdrv_meta::DynamicMeta::new(0, 1000.0, 200.0);
        qdrv_meta::to_json(&dynamic).unwrap()
    }

    fn qdrv32_container(dynamic: qdrv_meta::DynamicMeta, payload: &[u8]) -> Vec<u8> {
        let static_meta = qdrv_meta::StaticMeta::default_delivery(1000.0, 400.0);
        let static_json = qdrv_meta::to_json(&static_meta).unwrap();
        let dynamic_json = qdrv_meta::to_json(&dynamic).unwrap();
        let mut out = vec![0u8; QDRV_HEADER_SIZE];
        out[0..4].copy_from_slice(QDRV_MAGIC);
        out[4..6].copy_from_slice(&CONTAINER_VERSION_V2.to_le_bytes());
        out[6] = TIER_DELIVERY;
        out[7] = CODEC_AV1;
        out[8..12].copy_from_slice(&2u32.to_le_bytes());
        out[12..16].copy_from_slice(&2u32.to_le_bytes());
        out[16..20].copy_from_slice(&1u32.to_le_bytes());
        out[20..24].copy_from_slice(&0u32.to_le_bytes());
        out[24..28].copy_from_slice(&(static_json.len() as u32).to_le_bytes());
        out.extend_from_slice(static_json.as_bytes());
        out.extend_from_slice(&(dynamic_json.len() as u32).to_le_bytes());
        out.extend_from_slice(dynamic_json.as_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn tone_maps_a_small_frame() {
        let json = sample_dynamic_json();
        let pq_rgb = vec![0.5f32; 2 * 2 * 3];
        let out = tone_map_frame_core(&pq_rgb, 2, 2, &json, None, 600.0, 0.1).unwrap();
        assert_eq!(out.len(), pq_rgb.len());
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn rejects_mismatched_pixel_buffer() {
        let json = sample_dynamic_json();
        let pq_rgb = vec![0.5f32; 5]; // not width*height*3
        assert!(tone_map_frame_core(&pq_rgb, 2, 2, &json, None, 600.0, 0.1).is_err());
    }

    #[test]
    fn rejects_invalid_dynamic_metadata() {
        let pq_rgb = vec![0.5f32; 3];
        assert!(tone_map_frame_core(&pq_rgb, 1, 1, "not json", None, 600.0, 0.1).is_err());
    }

    #[test]
    fn yuv_neutral_chroma_yields_equal_rgb() {
        // Y'=0.5 with neutral chroma must produce R=G=B=0.5 (grey preserved).
        let y = vec![0.5f32; 4];
        let cb = vec![0.0f32; 4];
        let cr = vec![0.0f32; 4];
        let rgb = yuv_ncl_to_pq_rgb_core(&y, &cb, &cr, 2, 2).unwrap();
        assert_eq!(rgb.len(), 12);
        for chunk in rgb.chunks_exact(3) {
            assert!((chunk[0] - 0.5).abs() < 1e-6);
            assert!((chunk[1] - 0.5).abs() < 1e-6);
            assert!((chunk[2] - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn yuv_positive_cr_drives_red() {
        let rgb = yuv_ncl_to_pq_rgb_core(&[0.5], &[0.0], &[0.25], 1, 1).unwrap();
        // Cr lifts red above luma and above the other channels.
        assert!(rgb[0] > 0.5);
        assert!(rgb[0] > rgb[1]);
        assert!(rgb[0] > rgb[2]);
    }

    #[test]
    fn yuv_rejects_mismatched_planes() {
        let y = vec![0.5f32; 4];
        let cb = vec![0.0f32; 3]; // wrong length
        let cr = vec![0.0f32; 4];
        assert!(yuv_ncl_to_pq_rgb_core(&y, &cb, &cr, 2, 2).is_err());
    }

    #[test]
    fn tone_curve_lut_has_requested_size_and_is_monotone() {
        let json = sample_dynamic_json();
        let lut = build_tone_curve_lut_core(&json, 256).unwrap();
        assert_eq!(lut.len(), 256);
        assert!(lut.iter().all(|v| v.is_finite()));
        // Valid QDRV tone curves are monotone non-decreasing.
        assert!(lut.windows(2).all(|w| w[1] >= w[0] - 1e-4));
    }

    #[test]
    fn tone_curve_lut_rejects_tiny_size() {
        let json = sample_dynamic_json();
        assert!(build_tone_curve_lut_core(&json, 1).is_err());
    }

    // Minimal AV1 temporal unit (temporal delimiter, sequence header, frame
    // OBU) with one embedded QDRV metadata payload, matching what `qdrv mux`
    // produces.
    fn stream_with_one_metadata(meta: &qdrv_meta::DynamicMeta) -> Vec<u8> {
        let mut tu = Vec::new();
        tu.extend_from_slice(&[0x12, 0]); // temporal delimiter
        tu.extend_from_slice(&[0x0A, 3, 0xAA, 0xBB, 0xCC]); // sequence header
        tu.extend_from_slice(&[0x32, 4, 0x11, 0x22, 0x33, 0x44]); // frame OBU
        let payload = qdrv_meta::binary::encode_dynamic_binary(meta).unwrap();
        qdrv_codec::embed_qdrv_metadata(&tu, &payload).unwrap()
    }

    #[test]
    fn extracts_embedded_stream_metadata() {
        let meta = qdrv_meta::DynamicMeta::new(7, 1000.0, 200.0);
        let stream = stream_with_one_metadata(&meta);
        let json = extract_stream_metadata_core(&stream).unwrap();
        let recovered: Vec<qdrv_meta::DynamicMeta> = qdrv_meta::from_json(&json).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].frame_index, 7);
    }

    #[test]
    fn extracts_empty_for_stream_without_metadata() {
        // A temporal unit carrying no QDRV metadata OBU yields an empty array.
        let mut tu = Vec::new();
        tu.extend_from_slice(&[0x12, 0]);
        tu.extend_from_slice(&[0x32, 4, 0x11, 0x22, 0x33, 0x44]);
        let json = extract_stream_metadata_core(&tu).unwrap();
        let recovered: Vec<qdrv_meta::DynamicMeta> = qdrv_meta::from_json(&json).unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn parses_qdrv32_container_manifest() {
        let payload = [0x12, 0x00, 0x32, 0x01, 0xAA];
        let bytes = qdrv32_container(qdrv_meta::DynamicMeta::new(0, 1000.0, 200.0), &payload);
        let json = parse_qdrv32_container_core(&bytes).unwrap();
        let manifest: ManifestForTest = qdrv_meta::from_json(&json).unwrap();

        assert_eq!(manifest.width, 2);
        assert_eq!(manifest.height, 2);
        assert_eq!(manifest.frame_count, 1);
        assert_eq!(manifest.frames.len(), 1);
        let frame = &manifest.frames[0];
        assert_eq!(frame.frame_index, 0);
        assert_eq!(frame.dynamic_metadata.frame_index, 0);
        assert_eq!(frame.payload_len, payload.len());
        assert_eq!(
            &bytes[frame.payload_offset..frame.payload_offset + frame.payload_len],
            payload.as_slice()
        );
    }

    #[test]
    fn wraps_av1_still_picture_as_avif() {
        let payload = [0x12, 0x00, 0x32, 0x01, 0xAA];
        let avif = wrap_av1_still_as_avif_core(&payload, 2, 2).unwrap();
        assert!(avif.windows(4).any(|w| w == b"ftyp"));
        assert!(avif.windows(4).any(|w| w == b"avif"));
        assert!(avif.windows(4).any(|w| w == b"av01"));
        assert!(avif.windows(payload.len()).any(|w| w == payload));
    }

    #[test]
    fn qdrv32_parser_rejects_frame_index_mismatch() {
        let bytes = qdrv32_container(qdrv_meta::DynamicMeta::new(7, 1000.0, 200.0), &[0x12, 0]);
        let err = parse_qdrv32_container_core(&bytes).unwrap_err();
        assert!(err.contains("does not match stream position"));
    }

    #[test]
    fn qdrv32_parser_rejects_truncated_payload() {
        let mut bytes = qdrv32_container(qdrv_meta::DynamicMeta::new(0, 1000.0, 200.0), &[1, 2, 3]);
        bytes.pop();
        let err = parse_qdrv32_container_core(&bytes).unwrap_err();
        assert!(err.contains("truncated AV1 payload"));
    }

    #[test]
    fn qdrv32_parser_rejects_non_delivery_tier() {
        let mut bytes = qdrv32_container(qdrv_meta::DynamicMeta::new(0, 1000.0, 200.0), &[0x12, 0]);
        bytes[6] = 0;
        let err = parse_qdrv32_container_core(&bytes).unwrap_err();
        assert!(err.contains("delivery-tier .qdrv32 only"));
    }
}
