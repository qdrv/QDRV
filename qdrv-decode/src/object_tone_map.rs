// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Object-based tone mapping for QDRV delivery-tier streams.
//!
//! Extends the global tone mapping pipeline with per-region curve overrides.
//! For each pixel, the decoder checks whether the pixel falls within any
//! [`ObjectRegion`]'s bounding box. If so, the highest-priority region's
//! tone mapping curve is used; otherwise, the global frame curve applies.

use qdrv_core::{
    pixel::Pixel32,
    pq::{PQ_MAX_NITS, pq_eotf_f32, pq_oetf_f32},
};
use qdrv_meta::{DynamicMeta, ToneMapCurve, object_meta::ObjectMeta};

use crate::tone_map::{TargetDisplay, safe_pixel32, sanitise_target_range};

/// Errors produced by [`tone_map_frame_with_objects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectToneMapError {
    /// `object_meta.frame_index` did not match `dynamic.frame_index`. The
    /// per-frame object metadata is documented to be aligned with the
    /// global `DynamicMeta` for the same frame; mismatched indices indicate
    /// that the wrong object table was supplied for this frame and the
    /// renderer refuses to apply it rather than silently using stale
    /// regions.
    FrameIndexMismatch {
        /// Frame index from the global per-frame dynamic metadata.
        dynamic_frame_index: u64,
        /// Frame index from the supplied object metadata.
        object_frame_index: u64,
    },
}

impl std::fmt::Display for ObjectToneMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectToneMapError::FrameIndexMismatch {
                dynamic_frame_index,
                object_frame_index,
            } => write!(
                f,
                "object metadata frame_index {object_frame_index} does not match \
                 dynamic metadata frame_index {dynamic_frame_index}"
            ),
        }
    }
}

impl std::error::Error for ObjectToneMapError {}

/// Applies object-based display-adaptive tone mapping to a buffer of QDRV
/// delivery-tier pixels.
///
/// For each pixel, the decoder resolves the tone mapping curve from the
/// object metadata (if the pixel falls within a region) or falls back to
/// the global curve from the dynamic metadata.
///
/// # Arguments
/// * `pixels`      — Delivery-tier PQ-encoded pixels, row-major.
/// * `width`       — Frame width in pixels.
/// * `height`      — Frame height in pixels.
/// * `dynamic`     — Per-frame dynamic metadata (global curve + scene stats).
/// * `object_meta` — Optional per-frame object metadata with regional curves.
///   When supplied, its `frame_index` MUST equal `dynamic.frame_index`
///   (audit finding K-1).
/// * `target`      — Capabilities of the target display.
///
/// # Errors
/// Returns [`ObjectToneMapError::FrameIndexMismatch`] if `object_meta` is
/// supplied with a `frame_index` that does not match `dynamic.frame_index`.
pub fn tone_map_frame_with_objects(
    pixels: &[Pixel32],
    width: u32,
    height: u32,
    dynamic: &DynamicMeta,
    object_meta: Option<&ObjectMeta>,
    target: &TargetDisplay,
) -> Result<Vec<Pixel32>, ObjectToneMapError> {
    // K-1 enforcement: `ObjectMeta` documents that its `frame_index` must
    // match the parent `DynamicMeta`. Previously this contract lived only
    // in the doc comment; now the renderer refuses mismatched pairs at the
    // boundary so callers get a clear diagnostic instead of silently using
    // the wrong object regions against the wrong frame.
    if let Some(om) = object_meta
        && om.frame_index != dynamic.frame_index
    {
        return Err(ObjectToneMapError::FrameIndexMismatch {
            dynamic_frame_index: dynamic.frame_index,
            object_frame_index: om.frame_index,
        });
    }

    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }
    let inv_w = 1.0 / w as f32;
    let inv_h = 1.0 / h as f32;
    let ref_max = dynamic.target_display_hint.max_luminance_nits;
    let (target_min_nits, target_max_nits) = sanitise_target_range(target);

    Ok(pixels
        .iter()
        .enumerate()
        .map(|(i, p)| {
            // Row-major raster order: index `i` counts along rows, so `i % width` is the
            // column and `i / width` is the row. Dividing each by the frame extent maps
            // pixel centres into normalised texture coordinates in `[0.0, 1.0)` for
            // object metadata lookup (horizontal `col`, vertical `row`).
            let col = (i % w) as f32 * inv_w;
            let row = (i / w) as f32 * inv_h;

            let curve = object_meta
                .and_then(|om| om.resolve_curve_at(col, row))
                .unwrap_or(&dynamic.tone_map_curve);

            let r = map_channel(p.r, curve, ref_max, target_min_nits, target_max_nits);
            let g = map_channel(p.g, curve, ref_max, target_min_nits, target_max_nits);
            let b = map_channel(p.b, curve, ref_max, target_min_nits, target_max_nits);
            safe_pixel32(r, g, b)
        })
        .collect())
}

#[inline]
fn map_channel(
    pq_in: f32,
    curve: &ToneMapCurve,
    ref_max: f32,
    target_min_nits: f32,
    target_max_nits: f32,
) -> f32 {
    let linear_nits = pq_eotf_f32(pq_in) * PQ_MAX_NITS as f32;
    let normalised = (linear_nits / ref_max.max(1.0)).clamp(0.0, 1.0);
    let mapped = curve.evaluate(normalised);
    let output_nits = (mapped * target_max_nits).clamp(target_min_nits, target_max_nits);
    pq_oetf_f32(output_nits / PQ_MAX_NITS as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrv_meta::{
        DynamicMeta, ToneMapCurve,
        object_meta::{BoundingBox, ObjectMeta, ObjectRegion},
    };

    /// Ensures object-aware tone mapping degrades cleanly when no object metadata is
    /// supplied: every pixel is processed and the output buffer length matches input.
    #[test]
    fn test_object_tone_map_no_regions() {
        let dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 4];
        let target = TargetDisplay::default();
        let result = tone_map_frame_with_objects(&pixels, 2, 2, &dynamic, None, &target)
            .expect("no object meta path must succeed");
        assert_eq!(result.len(), 4);
    }

    /// Checks that a higher-priority object region's tone curve measurably overrides the
    /// global frame curve for pixels inside that region's bounding box (top-left sample).
    #[test]
    fn test_object_tone_map_region_overrides() {
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.tone_map_curve = ToneMapCurve::linear();

        let region_curve = ToneMapCurve::default_1000nit();
        let object_meta = ObjectMeta {
            frame_index: 0,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
                tone_map_curve: region_curve.clone(),
                priority: 10,
            }],
        };

        let pixels = vec![Pixel32::new_unchecked(0.4, 0.4, 0.4); 4];
        let target = TargetDisplay::default();

        let with_obj =
            tone_map_frame_with_objects(&pixels, 2, 2, &dynamic, Some(&object_meta), &target)
                .expect("matched-index object meta must succeed");
        let without = tone_map_frame_with_objects(&pixels, 2, 2, &dynamic, None, &target)
            .expect("no-object path must succeed");

        // The top-left pixel (0,0) falls within the region and should use the
        // Bezier curve, producing a different result than the global linear curve.
        let diff = (with_obj[0].r - without[0].r).abs();
        assert!(diff > 1e-4, "object region had no effect: diff={diff}");
    }

    #[test]
    fn test_object_tone_map_invalid_target_range_is_sanitised() {
        let dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5)];
        let target = TargetDisplay {
            min_nits: 500.0,
            max_nits: 100.0,
        };

        let result = tone_map_frame_with_objects(&pixels, 1, 1, &dynamic, None, &target)
            .expect("sanitised target path must succeed");
        assert_eq!(result.len(), 1);
        assert!(result[0].r.is_finite());
    }

    /// K-1 regression: mismatched `ObjectMeta.frame_index` vs
    /// `DynamicMeta.frame_index` must be rejected at the renderer boundary
    /// instead of silently using stale object regions against the wrong
    /// frame.
    #[test]
    fn test_object_tone_map_rejects_frame_index_mismatch() {
        let dynamic = DynamicMeta::new(7, 1000.0, 200.0);
        let object_meta = ObjectMeta {
            frame_index: 3, // intentionally different from dynamic.frame_index
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 10,
            }],
        };
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 4];
        let target = TargetDisplay::default();
        let err = tone_map_frame_with_objects(&pixels, 2, 2, &dynamic, Some(&object_meta), &target)
            .expect_err("mismatched frame_index must be rejected");
        assert!(matches!(
            err,
            ObjectToneMapError::FrameIndexMismatch {
                dynamic_frame_index: 7,
                object_frame_index: 3,
            }
        ));
    }
}
