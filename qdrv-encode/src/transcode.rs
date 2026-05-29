// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Mastering-to-delivery tier transcoding for QDRV.
//!
//! This module converts Float64 linear light mastering-tier pixels to Float32
//! SMPTE ST 2084 PQ-encoded delivery-tier pixels, and generates the
//! corresponding per-frame SMPTE ST 2094-based dynamic metadata.
//!
//! ## Transcode pipeline
//!
//! For each frame:
//! 1. Compute scene luminance statistics (peak and average nits) using the
//!    ITU-R Rec. 2100 non-constant luminance coefficients.
//! 2. Normalise each linear light channel to `[0.0, 1.0]` by dividing by
//!    `PQ_MAX_NITS` (10 000).
//! 3. Apply the SMPTE ST 2084 PQ OETF at Float32 precision.
//! 4. Generate per-frame SMPTE ST 2094-based dynamic metadata from the scene
//!    luminance statistics.
//!
//! A simultaneous HDR10-compatible 10-bit integer output can be derived from
//! the delivery-tier pixels using [`to_hdr10_10bit`].

use qdrv_core::{
    colors::ncl::{KB, KG, KR},
    pixel::{Pixel32, Pixel64},
    pq::{PQ_MAX_NITS, pq_oetf_f32},
};
use qdrv_meta::{
    DynamicMeta, StaticMeta,
    compatibility::METADATA_SCHEMA_V2,
    open_dynamic_v2::{InverseToneMappingHint, OpenDynamicMetadataV2},
};
use thiserror::Error;

/// Errors produced by `qdrv-encode` transcoding operations.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// The pixel buffer passed to a transcode function was empty.
    #[error("pixel buffer is empty")]
    EmptyBuffer,
    /// A mastering pixel channel contained NaN or infinity.
    #[error("non-finite mastering pixel at index {index} channel {channel}: {value}")]
    NonFinitePixel {
        index: usize,
        channel: &'static str,
        value: f64,
    },
    /// Scene luminance statistics evaluated to a non-finite number.
    #[error("non-finite scene statistics: peak={peak}, avg={avg}")]
    NonFiniteSceneStats { peak: f32, avg: f32 },
    /// A core pixel or PQ operation failed.
    #[error("core error: {0}")]
    Core(#[from] qdrv_core::QdrvError),
}

/// The output of a successful mastering-to-delivery tier transcode operation.
pub struct TranscodeResult {
    /// Delivery-tier pixels: Float32, SMPTE ST 2084 PQ-encoded, Rec. 2100.
    pub pixels: Vec<Pixel32>,
    /// Static stream metadata to be embedded in the container header.
    pub static_meta: StaticMeta,
    /// Per-frame dynamic metadata containing ST 2094-based tone curve and
    /// scene luminance statistics.
    pub dynamic_meta: DynamicMeta,
}

/// Optional controls for deterministic and policy-aware transcoding.
#[derive(Debug, Clone, Default)]
pub struct EncodeOptions {
    /// Enables deterministic quantisation in the encode path so repeated
    /// runs produce stable intermediate PQ sample values.
    pub deterministic: bool,
    /// Enables creator intent lock flag in generated metadata.
    pub creator_intent_locked: bool,
    /// Optional Open Dynamic Metadata v2 payload.
    pub open_dynamic_v2: Option<OpenDynamicMetadataV2>,
    /// Optional inverse SDR->HDR reconstruction hints.
    pub inverse_tone_mapping_hint: Option<InverseToneMappingHint>,
}

/// Transcodes a buffer of mastering-tier pixels (Float64 linear light) to
/// delivery-tier pixels (Float32 SMPTE ST 2084 PQ-encoded).
///
/// See the [module documentation](self) for a description of the full pipeline.
///
/// # Arguments
/// * `pixels`      — Mastering-tier pixel buffer in linear light (nits).
/// * `frame_index` — Zero-based index of this frame within the stream.
///   This value is stored verbatim in the returned `DynamicMeta` and must
///   equal the frame's eventual position when written through
///   [`qdrv_io::writer::write_delivery_file`] — the QDRV reader enforces
///   the sequencing contract and rejects mismatched indices.
/// * `static_meta` — Static stream metadata to associate with this output.
///
/// # Errors
/// Returns [`EncodeError::EmptyBuffer`] if `pixels` is empty.
pub fn transcode_frame(
    pixels: &[Pixel64],
    frame_index: u64,
    static_meta: StaticMeta,
) -> Result<TranscodeResult, EncodeError> {
    transcode_frame_with_options(pixels, frame_index, static_meta, &EncodeOptions::default())
}

/// Variant of [`transcode_frame`] with explicit encode options.
pub fn transcode_frame_with_options(
    pixels: &[Pixel64],
    frame_index: u64,
    static_meta: StaticMeta,
    options: &EncodeOptions,
) -> Result<TranscodeResult, EncodeError> {
    if pixels.is_empty() {
        return Err(EncodeError::EmptyBuffer);
    }
    validate_mastering_pixels(pixels)?;

    // Step 1: Compute scene luminance statistics using Rec. 2100 NCL coefficients.
    let (scene_peak, scene_avg) = compute_luminance_stats(pixels);
    if !scene_peak.is_finite() || !scene_avg.is_finite() {
        return Err(EncodeError::NonFiniteSceneStats {
            peak: scene_peak,
            avg: scene_avg,
        });
    }

    // Steps 2 and 3: Normalise to [0.0, 1.0] and apply the ST 2084 PQ OETF.
    let delivery_pixels: Vec<Pixel32> = pixels
        .iter()
        .map(|p| {
            let mut r = pq_oetf_f32((p.r / PQ_MAX_NITS).clamp(0.0, 1.0) as f32);
            let mut g = pq_oetf_f32((p.g / PQ_MAX_NITS).clamp(0.0, 1.0) as f32);
            let mut b = pq_oetf_f32((p.b / PQ_MAX_NITS).clamp(0.0, 1.0) as f32);
            if options.deterministic {
                r = deterministic_quantize(r);
                g = deterministic_quantize(g);
                b = deterministic_quantize(b);
            }
            Pixel32::new_unchecked(r, g, b)
        })
        .collect();

    // Step 4: Generate per-frame ST 2094-based dynamic metadata.
    let mut dynamic_meta = DynamicMeta::new(frame_index, scene_peak, scene_avg);
    dynamic_meta.open_dynamic_v2 = options.open_dynamic_v2.clone();
    dynamic_meta.inverse_tone_mapping_hint = options.inverse_tone_mapping_hint.clone();
    dynamic_meta.creator_intent_locked = options.creator_intent_locked;
    if dynamic_meta.open_dynamic_v2.is_some() {
        dynamic_meta.metadata_schema_version = METADATA_SCHEMA_V2;
    }

    Ok(TranscodeResult {
        pixels: delivery_pixels,
        static_meta,
        dynamic_meta,
    })
}

#[inline]
fn deterministic_quantize(v: f32) -> f32 {
    ((v.clamp(0.0, 1.0) * 65_535.0).round() / 65_535.0).clamp(0.0, 1.0)
}

fn validate_mastering_pixels(pixels: &[Pixel64]) -> Result<(), EncodeError> {
    for (index, p) in pixels.iter().enumerate() {
        for (channel, value) in [("r", p.r), ("g", p.g), ("b", p.b)] {
            if !value.is_finite() {
                return Err(EncodeError::NonFinitePixel {
                    index,
                    channel,
                    value,
                });
            }
        }
    }
    Ok(())
}

/// Generates a simultaneous HDR10-compatible output by quantising Float32
/// delivery-tier pixels to 10-bit integer values in `[0, 1023]`.
///
/// This is the same quantisation step that all existing HDR10 content
/// undergoes when mastered in a floating-point pipeline. No tone mapping is
/// applied; the full PQ signal range is preserved within 10-bit precision.
/// Any QDRV delivery stream can produce a conformant HDR10 sidecar output
/// using this function.
///
/// Output values are packed as `[R, G, B]` triplets in `u16`.
pub fn to_hdr10_10bit(pixels: &[Pixel32]) -> Vec<[u16; 3]> {
    pixels
        .iter()
        .map(|p| {
            let r = (p.r.clamp(0.0, 1.0) * 1023.0).round() as u16;
            let g = (p.g.clamp(0.0, 1.0) * 1023.0).round() as u16;
            let b = (p.b.clamp(0.0, 1.0) * 1023.0).round() as u16;
            [r, g, b]
        })
        .collect()
}

/// Computes the peak and average scene luminance in nits from a
/// mastering-tier pixel buffer.
///
/// Uses the ITU-R Rec. 2100 non-constant luminance coefficients from
/// [`qdrv_core::colors::ncl`]: `L = KR·R + KG·G + KB·B`.
///
/// Negative luminance (from out-of-gamut pixels) is clamped to zero before
/// accumulation to prevent distortion of scene statistics.
fn compute_luminance_stats(pixels: &[Pixel64]) -> (f32, f32) {
    let mut peak = 0.0_f64;
    let mut sum = 0.0_f64;

    for p in pixels {
        let lum = (KR * p.r + KG * p.g + KB * p.b).max(0.0);
        if lum > peak {
            peak = lum;
        }
        sum += lum;
    }

    let avg = sum / pixels.len() as f64;
    (peak as f32, avg as f32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use qdrv_core::pq::pq_eotf_f64;

    #[test]
    fn test_transcode_preserves_luminance() {
        // A neutral grey pixel at exactly 1 000 nits must encode to a PQ value
        // that, when decoded, recovers the original luminance within 0.01 nits.
        let nits = 1000.0_f64;
        let pixels = vec![Pixel64::new_unchecked(nits, nits, nits)];
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let result = transcode_frame(&pixels, 0, meta).unwrap();

        let p = &result.pixels[0];
        let decoded_nits = pq_eotf_f64(p.r as f64) * PQ_MAX_NITS;
        assert!(
            (decoded_nits - nits).abs() < 0.01,
            "Expected ~{nits} nits after PQ roundtrip, decoded {decoded_nits:.4}"
        );
    }

    #[test]
    fn test_transcode_empty_errors() {
        // Passing an empty pixel buffer must produce an EmptyBuffer error.
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let result = transcode_frame(&[], 0, meta);
        assert!(result.is_err());
    }

    #[test]
    fn test_hdr10_quantisation_range() {
        // All quantised output values must be within the valid 10-bit HDR10
        // range of [0, 1023].
        let pixels = vec![
            Pixel32::new_unchecked(0.0, 0.5, 1.0),
            Pixel32::new_unchecked(0.25, 0.75, 0.9),
        ];
        let quantised = to_hdr10_10bit(&pixels);
        for entry in &quantised {
            assert!(entry[0] <= 1023);
            assert!(entry[1] <= 1023);
            assert!(entry[2] <= 1023);
        }
        // 0.0 must quantise to 0 and 1.0 must quantise to 1023.
        assert_eq!(quantised[0][0], 0);
        assert_eq!(quantised[0][2], 1023);
    }
}
