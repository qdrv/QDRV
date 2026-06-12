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
use qdrv_meta::{
    DynamicMeta, ToneMapCurve,
    object_meta::{ObjectMeta, SphericalProjection},
};
use std::f32::consts::{FRAC_PI_4, PI, TAU};

use crate::tone_map::{TargetDisplay, safe_pixel32, sanitise_target_range};

/// Errors produced by [`tone_map_frame_with_objects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectToneMapError {
    /// `object_meta.frame_index` could not be applied to `dynamic.frame_index`.
    /// Same-frame metadata is always accepted. Cross-frame metadata is accepted
    /// only when every flat region carries an active bounded motion descriptor.
    FrameIndexMismatch {
        /// Frame index from the global per-frame dynamic metadata.
        dynamic_frame_index: u64,
        /// Authored keyframe index from the supplied object metadata.
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
    if let Some(om) = object_meta
        && !om.applies_to_frame(dynamic.frame_index)
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

    // 360°/immersive: a stream-level projection (if declared) tells the
    // renderer to interpret each pixel on the unit sphere for spherical region
    // lookup. Absent a projection, the flat raster path is used.
    let projection = dynamic
        .open_dynamic_v2
        .as_ref()
        .and_then(|v2| v2.spherical_projection);

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
                .and_then(|om| resolve_region_curve(om, dynamic.frame_index, projection, col, row))
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

/// Resolves the per-region tone-map curve for a pixel, choosing the spherical
/// or flat path based on whether a [`SphericalProjection`] is in effect.
fn resolve_region_curve(
    om: &ObjectMeta,
    frame_index: u64,
    projection: Option<SphericalProjection>,
    col: f32,
    row: f32,
) -> Option<&ToneMapCurve> {
    // 360°/immersive path: when a projection is declared and the frame carries
    // spherical regions, interpret the pixel on the unit sphere. Flat regions
    // are not consulted under a spherical projection (disjoint region spaces).
    if let Some(proj) = projection
        && !om.spherical_regions.is_empty()
    {
        let (azimuth, elevation) = raster_to_sphere(proj, col, row);
        return om.resolve_spherical_curve_at(azimuth, elevation);
    }
    om.resolve_curve_at_frame(frame_index, col, row)
}

/// Maps a normalised raster coordinate `(nx, ny)` in `[0, 1]` to a spherical
/// coordinate `(azimuth, elevation)` in radians under `projection`.
///
fn raster_to_sphere(projection: SphericalProjection, nx: f32, ny: f32) -> (f32, f32) {
    match projection {
        SphericalProjection::Equirectangular => {
            // Longitude spans [-pi, pi] left->right; latitude spans
            // [+pi/2, -pi/2] top->bottom (image row 0 is the north pole).
            let azimuth = (nx - 0.5) * TAU;
            let elevation = (0.5 - ny) * PI;
            (azimuth, elevation)
        }
        SphericalProjection::Cubemap => cubemap_raster_to_sphere(nx, ny, false),
        SphericalProjection::EquiAngularCubemap => cubemap_raster_to_sphere(nx, ny, true),
    }
}

/// Maps a normalised raster coordinate `(nx, ny)` to `(azimuth, elevation)` for
/// the canonical QDRV 3x2 cubemap layout (3 columns x 2 rows):
///
/// ```text
///   row 0:  +X  +Y  +Z
///   row 1:  -X  -Y  -Z
/// ```
///
/// Face-local coordinates run in `[-1, 1]`; when `equi_angular` is set the EAC
/// warp (`tan` of an angle linear in the pixel) is applied so the pixel grid
/// samples equal angles. Face orientation follows the standard cubemap
/// convention; azimuth 0 is `+Z` (frame-forward) and elevation `+pi/2` is `+Y`
/// (up), matching the equirectangular convention.
fn cubemap_raster_to_sphere(nx: f32, ny: f32, equi_angular: bool) -> (f32, f32) {
    let fc = ((nx * 3.0) as usize).min(2); // face column 0..=2
    let fr = ((ny * 2.0) as usize).min(1); // face row 0..=1
    let mut u = (nx * 3.0 - fc as f32) * 2.0 - 1.0;
    let mut v = (ny * 2.0 - fr as f32) * 2.0 - 1.0;
    if equi_angular {
        u = (u * FRAC_PI_4).tan();
        v = (v * FRAC_PI_4).tan();
    }
    // Face-local (u, v) -> 3D direction (standard cubemap convention).
    let (x, y, z) = match fr * 3 + fc {
        0 => (1.0, -v, -u),  // +X
        1 => (u, 1.0, v),    // +Y
        2 => (u, -v, 1.0),   // +Z
        3 => (-1.0, -v, u),  // -X
        4 => (u, -1.0, -v),  // -Y
        _ => (-u, -v, -1.0), // -Z
    };
    let azimuth = x.atan2(z);
    let elevation = y.atan2((x * x + z * z).sqrt());
    (azimuth, elevation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrv_meta::{
        DynamicMeta, ToneMapCurve,
        object_meta::{BoundingBox, ObjectMeta, ObjectRegion, RegionMotion, SphericalRegion},
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
                motion: None,
            }],
            spherical_regions: Vec::new(),
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
                motion: None,
            }],
            spherical_regions: Vec::new(),
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

    #[test]
    fn test_object_tone_map_applies_bounded_motion_from_keyframe() {
        let mut dynamic = DynamicMeta::new(2, 1000.0, 200.0);
        dynamic.tone_map_curve = ToneMapCurve::linear();

        let object_meta = ObjectMeta {
            frame_index: 0,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 0.25,
                    height: 1.0,
                },
                tone_map_curve: ToneMapCurve::default_1000nit(),
                priority: 10,
                motion: Some(RegionMotion::Translate {
                    dx_per_frame: 0.25,
                    dy_per_frame: 0.0,
                    frame_count: 3,
                }),
            }],
            spherical_regions: Vec::new(),
        };

        let pixels = vec![Pixel32::new_unchecked(0.4, 0.4, 0.4); 4];
        let target = TargetDisplay::default();
        let with_motion =
            tone_map_frame_with_objects(&pixels, 4, 1, &dynamic, Some(&object_meta), &target)
                .expect("active motion metadata should apply to frame 2");
        let without = tone_map_frame_with_objects(&pixels, 4, 1, &dynamic, None, &target)
            .expect("no-object path must succeed");

        let moved_region_diff = (with_motion[2].r - without[2].r).abs();
        assert!(
            moved_region_diff > 1e-4,
            "translated region had no effect: {moved_region_diff}"
        );
        let stale_origin_diff = (with_motion[0].r - without[0].r).abs();
        assert!(
            stale_origin_diff < 1e-6,
            "authored-frame origin should no longer match: {stale_origin_diff}"
        );
    }

    #[test]
    fn test_object_tone_map_rejects_expired_motion_keyframe() {
        let dynamic = DynamicMeta::new(3, 1000.0, 200.0);
        let object_meta = ObjectMeta {
            frame_index: 0,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 0.25,
                    height: 1.0,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 10,
                motion: Some(RegionMotion::Static { frame_count: 3 }),
            }],
            spherical_regions: Vec::new(),
        };
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 4];
        let target = TargetDisplay::default();
        let err = tone_map_frame_with_objects(&pixels, 4, 1, &dynamic, Some(&object_meta), &target)
            .expect_err("expired motion metadata must not be applied silently");
        assert!(matches!(
            err,
            ObjectToneMapError::FrameIndexMismatch {
                dynamic_frame_index: 3,
                object_frame_index: 0,
            }
        ));
    }

    /// Roadmap #1: under an equirectangular projection, a spherical region
    /// overrides the global curve for pixels whose sphere coordinate falls
    /// inside it, while pixels outside it are unaffected.
    #[test]
    fn test_spherical_region_applies_under_equirectangular() {
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.tone_map_curve = ToneMapCurve::linear();
        let v2: qdrv_meta::OpenDynamicMetadataV2 =
            qdrv_meta::from_json(r#"{"spherical_projection":"equirectangular"}"#)
                .expect("v2 json must parse");
        dynamic.open_dynamic_v2 = Some(v2);

        let object_meta = ObjectMeta {
            frame_index: 0,
            regions: Vec::new(),
            spherical_regions: vec![SphericalRegion {
                id: 1,
                centre_azimuth: 0.0,
                centre_elevation: 0.0,
                angular_width: std::f32::consts::FRAC_PI_2,
                angular_height: std::f32::consts::FRAC_PI_2,
                tone_map_curve: ToneMapCurve::default_1000nit(),
                priority: 10,
            }],
        };

        let pixels = vec![Pixel32::new_unchecked(0.4, 0.4, 0.4); 16];
        let target = TargetDisplay::default();
        let out = tone_map_frame_with_objects(&pixels, 4, 4, &dynamic, Some(&object_meta), &target)
            .expect("spherical path must succeed");
        let flat = tone_map_frame_with_objects(&pixels, 4, 4, &dynamic, None, &target)
            .expect("no-object path must succeed");

        // Pixel index 10 -> col=0.5, row=0.5 -> (az=0, el=0): inside the region.
        let diff_centre = (out[10].r - flat[10].r).abs();
        assert!(
            diff_centre > 1e-4,
            "spherical region had no effect at frame centre: {diff_centre}"
        );
        // Pixel index 0 -> col=0, row=0 -> (az=-pi, el=+pi/2): outside the region.
        let diff_corner = (out[0].r - flat[0].r).abs();
        assert!(
            diff_corner < 1e-6,
            "corner pixel should be unaffected: {diff_corner}"
        );
    }

    /// Roadmap #1: the cubemap projection un-projects each pixel through the
    /// canonical 3x2 face grid, so a spherical region centred frame-forward
    /// applies to the centre of the `+Z` face.
    #[test]
    fn test_spherical_region_applies_under_cubemap() {
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.tone_map_curve = ToneMapCurve::linear();
        let v2: qdrv_meta::OpenDynamicMetadataV2 =
            qdrv_meta::from_json(r#"{"spherical_projection":"cubemap"}"#)
                .expect("v2 json must parse");
        dynamic.open_dynamic_v2 = Some(v2);

        let object_meta = ObjectMeta {
            frame_index: 0,
            regions: Vec::new(),
            spherical_regions: vec![SphericalRegion {
                id: 1,
                centre_azimuth: 0.0,
                centre_elevation: 0.0,
                angular_width: std::f32::consts::FRAC_PI_2,
                angular_height: std::f32::consts::FRAC_PI_2,
                tone_map_curve: ToneMapCurve::default_1000nit(),
                priority: 10,
            }],
        };

        // 12x4 frame: the +Z face is columns 8..12, rows 0..2; pixel index 22
        // (col 10, row 1) maps to the +Z face centre -> (az=0, el=0), inside.
        let pixels = vec![Pixel32::new_unchecked(0.4, 0.4, 0.4); 48];
        let target = TargetDisplay::default();
        let out =
            tone_map_frame_with_objects(&pixels, 12, 4, &dynamic, Some(&object_meta), &target)
                .expect("cubemap path must succeed");
        let flat = tone_map_frame_with_objects(&pixels, 12, 4, &dynamic, None, &target)
            .expect("no-object path must succeed");
        let diff = (out[22].r - flat[22].r).abs();
        assert!(
            diff > 1e-4,
            "cubemap spherical region had no effect: {diff}"
        );
    }
}
