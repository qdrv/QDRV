// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Object-based tone mapping metadata for QDRV.
//!
//! Defines per-object tone mapping regions within a frame. Unlike
//! frame-global tone mapping (which applies a single curve to the entire
//! frame), object-based metadata allows different tone mapping curves for
//! different spatial regions — enabling scene-aware dynamic-range processing where,
//! for example, a bright window and a dark interior can be tone-mapped
//! independently.
//!
//! ## Design
//!
//! Each frame may carry zero or more [`ObjectRegion`] entries alongside
//! the global [`DynamicMeta`]. Each region defines a normalised bounding
//! box and its own [`ToneMapCurve`]. Regions are evaluated in priority
//! order; overlapping regions are resolved by the highest-priority region.
//!
//! For 360°/immersive content, a frame may instead carry [`SphericalRegion`]
//! entries, which locate a region by angular coordinates on the unit sphere
//! rather than by a flat raster [`BoundingBox`]. The stream's
//! [`SphericalProjection`] — carried once at the scene level on
//! [`crate::open_dynamic_v2::OpenDynamicMetadataV2`] — describes how the flat
//! delivery raster maps onto the sphere.
//!
//! This extension is compatible with the SMPTE ST 2094 framework, which
//! reserves provisions for region-based processing.

use crate::tone_curve::ToneMapCurve;
use serde::{Deserialize, Serialize};
use std::f32::consts::{FRAC_PI_2, PI, TAU};

/// Maximum number of control points accepted by a piecewise-linear region
/// motion descriptor.
pub const MAX_REGION_MOTION_KEYFRAMES: usize = 64;

/// A normalised axis-aligned bounding box within a frame.
///
/// All coordinates are in `[0.0, 1.0]` relative to the frame dimensions.
/// `(0.0, 0.0)` is the top-left corner; `(1.0, 1.0)` is the bottom-right.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundingBox {
    /// Left edge of the region, normalised to `[0.0, 1.0]`.
    pub x: f32,
    /// Top edge of the region, normalised to `[0.0, 1.0]`.
    pub y: f32,
    /// Width of the region, normalised.
    pub width: f32,
    /// Height of the region, normalised.
    pub height: f32,
}

/// Offset keyframe for piecewise-linear rectilinear region motion.
///
/// `frame_delta` is measured from the parent [`ObjectMeta::frame_index`].
/// `dx` and `dy` are normalised-coordinate offsets applied to the region's
/// authored [`BoundingBox`] origin.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionKeyframe {
    /// Frame offset from the authored keyframe.
    pub frame_delta: u32,
    /// Horizontal normalised-coordinate offset at this keyframe.
    pub dx: f32,
    /// Vertical normalised-coordinate offset at this keyframe.
    pub dy: f32,
}

/// Motion descriptor applied to a rectilinear object region across a bounded
/// frame span.
///
/// Motion is measured from the parent [`ObjectMeta::frame_index`]. A descriptor
/// is active for `frame_count` frames including the authored keyframe itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegionMotion {
    /// Keep the region fixed for a bounded number of frames.
    Static {
        /// Number of frames for which this descriptor applies, including the
        /// authored keyframe.
        frame_count: u32,
    },
    /// Translate the region by a fixed normalised delta per frame.
    Translate {
        /// Horizontal normalised-coordinate delta per rendered frame.
        dx_per_frame: f32,
        /// Vertical normalised-coordinate delta per rendered frame.
        dy_per_frame: f32,
        /// Number of frames for which this descriptor applies, including the
        /// authored keyframe.
        frame_count: u32,
    },
    /// Interpolate explicit normalised offsets between bounded keyframes.
    PiecewiseLinear {
        /// Strictly increasing offset keyframes. The first keyframe must be
        /// frame 0 with zero offset because the region's bounding box is the
        /// authored keyframe position.
        keyframes: Vec<MotionKeyframe>,
    },
}

/// A spatial region within a frame with its own tone mapping curve.
///
/// Multiple regions may overlap; the decoder resolves overlaps using the
/// `priority` field (higher values take precedence).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRegion {
    /// Unique identifier for this region within the frame.
    pub id: u32,
    /// Spatial bounding box of this region (normalised coordinates).
    pub bounding_box: BoundingBox,
    /// Per-region tone mapping curve. Overrides the global curve within
    /// this bounding box.
    pub tone_map_curve: ToneMapCurve,
    /// Priority for overlap resolution. Higher values take precedence.
    pub priority: u8,
    /// Optional bounded motion descriptor for resolving this region on frames
    /// after the authored keyframe.
    #[serde(default)]
    pub motion: Option<RegionMotion>,
}

/// Container-level projection used to interpret [`SphericalRegion`]
/// coordinates against a flat delivery raster.
///
/// A 360°/immersive stream stores a flat 2D frame whose pixels map onto the
/// unit sphere by a fixed projection. The projection is constant for the
/// stream, so it is carried once at the scene level on
/// [`crate::open_dynamic_v2::OpenDynamicMetadataV2`] rather than per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SphericalProjection {
    /// Equirectangular projection (ERP): longitude maps linearly to the
    /// horizontal axis, latitude linearly to the vertical axis.
    Equirectangular,
    /// Standard cubemap layout (six cube faces packed into the frame).
    Cubemap,
    /// Equi-angular cubemap (EAC): a cubemap with equal-angle sampling that
    /// reduces the corner oversampling of a standard cubemap.
    EquiAngularCubemap,
}

/// A region defined on the unit sphere, with its own tone mapping curve.
///
/// This is the spherical counterpart to [`ObjectRegion`]: it carries the
/// same `tone_map_curve` and `priority` fields, but locates the region by
/// angular coordinates rather than a flat [`BoundingBox`]. All angles are in
/// radians.
///
/// - `centre_azimuth` is longitude in `[-π, π]` (0 is frame-forward; the
///   axis is cyclic and wraps across the antimeridian at ±π).
/// - `centre_elevation` is latitude in `[-π/2, π/2]` (positive is up; ±π/2
///   are the poles).
/// - `angular_width` and `angular_height` are the *full* angular extents
///   (not half-extents), both strictly positive.
///
/// The latitude extent must stay within the poles; the longitude extent may
/// wrap across the antimeridian, so a full `2π` ring is representable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SphericalRegion {
    /// Unique identifier for this region within the frame.
    pub id: u32,
    /// Centre longitude in radians, `[-π, π]`.
    pub centre_azimuth: f32,
    /// Centre latitude in radians, `[-π/2, π/2]`.
    pub centre_elevation: f32,
    /// Full longitude extent in radians, `(0, 2π]`.
    pub angular_width: f32,
    /// Full latitude extent in radians, `(0, π]`.
    pub angular_height: f32,
    /// Per-region tone mapping curve. Overrides the global curve within
    /// this region.
    pub tone_map_curve: ToneMapCurve,
    /// Priority for overlap resolution. Higher values take precedence.
    pub priority: u8,
}

/// Per-frame object-based tone mapping metadata.
///
/// Extends [`DynamicMeta`](crate::DynamicMeta) with spatial region
/// information. When present, the decoder applies region-specific tone
/// curves to pixels within each [`ObjectRegion`]'s bounding box, falling
/// back to the global tone curve for pixels outside all regions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectMeta {
    /// Zero-based frame index (must match the parent `DynamicMeta`).
    pub frame_index: u64,
    /// Object regions, ordered by priority (highest first).
    pub regions: Vec<ObjectRegion>,
    /// Spherical regions for 360°/immersive content, interpreted under the
    /// stream's [`SphericalProjection`]. Empty for ordinary flat content.
    #[serde(default)]
    pub spherical_regions: Vec<SphericalRegion>,
}

impl BoundingBox {
    /// Returns `true` if the normalised pixel coordinate `(px, py)` falls
    /// within this bounding box. Both coordinates are in `[0.0, 1.0]`.
    #[inline]
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    /// Validates that this bounding box is geometrically well-formed.
    ///
    /// Per the `[0.0, 1.0]` normalised-coordinate contract, valid boxes
    /// have finite, non-negative origin, positive extent, and stay inside
    /// the unit square. Standalone construction of `BoundingBox` bypasses
    /// the cross-cutting validator in
    /// [`crate::open_dynamic_v2::OpenDynamicMetadataV2::validate`] (U-13),
    /// so this method exists for callers who own a box outside that
    /// context.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
        {
            return Err("bounding box values must be finite");
        }
        if self.x < 0.0 || self.y < 0.0 {
            return Err("bounding box origin must be non-negative");
        }
        if self.width <= 0.0 || self.height <= 0.0 {
            return Err("bounding box must have positive extent");
        }
        if self.x + self.width > 1.0 || self.y + self.height > 1.0 {
            return Err("bounding box must stay within the [0,1] unit square");
        }
        Ok(())
    }
}

impl RegionMotion {
    /// Returns the active span of this motion descriptor, in frames.
    pub fn frame_count(&self) -> u64 {
        match self {
            RegionMotion::Static { frame_count } | RegionMotion::Translate { frame_count, .. } => {
                u64::from(*frame_count)
            }
            RegionMotion::PiecewiseLinear { keyframes } => keyframes
                .last()
                .map_or(0, |keyframe| u64::from(keyframe.frame_delta) + 1),
        }
    }

    /// Returns the normalised offset at `frame_delta`, or `None` if the
    /// descriptor is not active for that delta.
    pub fn offset_at(&self, frame_delta: u64) -> Option<(f32, f32)> {
        if frame_delta >= self.frame_count() {
            return None;
        }
        match self {
            RegionMotion::Static { .. } => Some((0.0, 0.0)),
            RegionMotion::Translate {
                dx_per_frame,
                dy_per_frame,
                ..
            } => {
                let delta = frame_delta as f32;
                let dx = *dx_per_frame * delta;
                let dy = *dy_per_frame * delta;
                if dx.is_finite() && dy.is_finite() {
                    Some((dx, dy))
                } else {
                    None
                }
            }
            RegionMotion::PiecewiseLinear { keyframes } => {
                let target = u32::try_from(frame_delta).ok()?;
                for keyframe in keyframes {
                    if keyframe.frame_delta == target {
                        return finite_offset(keyframe.dx, keyframe.dy);
                    }
                }
                for pair in keyframes.windows(2) {
                    let [start, end] = pair else {
                        continue;
                    };
                    if start.frame_delta <= target && target <= end.frame_delta {
                        let span = end.frame_delta - start.frame_delta;
                        if span == 0 {
                            return None;
                        }
                        let t = (target - start.frame_delta) as f32 / span as f32;
                        let dx = start.dx + (end.dx - start.dx) * t;
                        let dy = start.dy + (end.dy - start.dy) * t;
                        return finite_offset(dx, dy);
                    }
                }
                None
            }
        }
    }

    /// Applies this motion descriptor to a bounding box at `frame_delta`.
    pub fn bounding_box_at(&self, base: BoundingBox, frame_delta: u64) -> Option<BoundingBox> {
        let (dx, dy) = self.offset_at(frame_delta)?;
        Some(BoundingBox {
            x: base.x + dx,
            y: base.y + dy,
            width: base.width,
            height: base.height,
        })
    }

    /// Validates motion fields and proves the bounded trajectory remains in the
    /// unit square for the supported linear path shapes.
    pub fn validate_for_box(&self, base: BoundingBox) -> Result<(), &'static str> {
        if self.frame_count() == 0 {
            return Err("region motion frame_count must be greater than zero");
        }

        match self {
            RegionMotion::Static { .. } => {}
            RegionMotion::Translate {
                dx_per_frame,
                dy_per_frame,
                ..
            } => {
                if !dx_per_frame.is_finite() || !dy_per_frame.is_finite() {
                    return Err("region motion deltas must be finite");
                }
                let last_delta = self.frame_count() - 1;
                let last_box = self
                    .bounding_box_at(base, last_delta)
                    .ok_or("region motion trajectory must be finite")?;
                last_box.validate()?;
            }
            RegionMotion::PiecewiseLinear { keyframes } => {
                validate_motion_keyframes(keyframes, base)?;
            }
        }
        Ok(())
    }
}

fn finite_offset(dx: f32, dy: f32) -> Option<(f32, f32)> {
    if dx.is_finite() && dy.is_finite() {
        Some((dx, dy))
    } else {
        None
    }
}

fn validate_motion_keyframes(
    keyframes: &[MotionKeyframe],
    base: BoundingBox,
) -> Result<(), &'static str> {
    if keyframes.len() < 2 {
        return Err("piecewise-linear region motion requires at least two keyframes");
    }
    if keyframes.len() > MAX_REGION_MOTION_KEYFRAMES {
        return Err("piecewise-linear region motion has too many keyframes");
    }

    let mut previous_delta: Option<u32> = None;
    for keyframe in keyframes {
        if !keyframe.dx.is_finite() || !keyframe.dy.is_finite() {
            return Err("piecewise-linear region motion offsets must be finite");
        }

        match previous_delta {
            None => {
                if keyframe.frame_delta != 0 || keyframe.dx != 0.0 || keyframe.dy != 0.0 {
                    return Err(
                        "piecewise-linear region motion must start at frame_delta 0 with zero offset",
                    );
                }
            }
            Some(previous) if keyframe.frame_delta <= previous => {
                return Err("piecewise-linear region motion keyframes must be strictly increasing");
            }
            Some(_) => {}
        }

        let box_at_keyframe = BoundingBox {
            x: base.x + keyframe.dx,
            y: base.y + keyframe.dy,
            width: base.width,
            height: base.height,
        };
        box_at_keyframe.validate()?;
        previous_delta = Some(keyframe.frame_delta);
    }
    Ok(())
}

impl ObjectRegion {
    /// Returns this region's bounding box at `frame_delta` from its authored
    /// keyframe, or `None` when the region is inactive for that frame.
    pub fn bounding_box_at_delta(&self, frame_delta: u64) -> Option<BoundingBox> {
        match &self.motion {
            Some(motion) => motion.bounding_box_at(self.bounding_box, frame_delta),
            None if frame_delta == 0 => Some(self.bounding_box),
            None => None,
        }
    }

    /// Validates this region's shape, tone curve, and optional bounded motion.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.bounding_box.validate()?;
        ToneMapCurve::validate_anchors_shape(&self.tone_map_curve.anchors, true)?;
        if let Some(motion) = &self.motion {
            motion.validate_for_box(self.bounding_box)?;
        }
        Ok(())
    }
}

impl SphericalRegion {
    /// Returns `true` if the spherical coordinate `(azimuth, elevation)`,
    /// both in radians, falls within this region.
    ///
    /// Elevation is tested as a plain interval around `centre_elevation`.
    /// Azimuth is tested cyclically: the shortest signed angular distance to
    /// `centre_azimuth` is compared against the half-width, so a region whose
    /// longitude extent crosses the ±π antimeridian still matches correctly.
    #[inline]
    pub fn contains(&self, azimuth: f32, elevation: f32) -> bool {
        let half_h = self.angular_height * 0.5;
        if elevation < self.centre_elevation - half_h || elevation > self.centre_elevation + half_h
        {
            return false;
        }
        let half_w = self.angular_width * 0.5;
        // Wrap the azimuth difference into [-π, π) so an extent spanning the
        // antimeridian is handled without special-casing the seam.
        let delta = (azimuth - self.centre_azimuth + PI).rem_euclid(TAU) - PI;
        delta.abs() <= half_w
    }

    /// Validates that the angular coordinates are finite, in range, and that
    /// the latitude extent stays within the poles.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.centre_azimuth.is_finite()
            || !self.centre_elevation.is_finite()
            || !self.angular_width.is_finite()
            || !self.angular_height.is_finite()
        {
            return Err("spherical region angles must be finite");
        }
        if !(-PI..=PI).contains(&self.centre_azimuth) {
            return Err("spherical region centre_azimuth must be in [-pi, pi]");
        }
        if !(-FRAC_PI_2..=FRAC_PI_2).contains(&self.centre_elevation) {
            return Err("spherical region centre_elevation must be in [-pi/2, pi/2]");
        }
        if self.angular_width <= 0.0 || self.angular_width > TAU {
            return Err("spherical region angular_width must be in (0, 2*pi]");
        }
        if self.angular_height <= 0.0 || self.angular_height > PI {
            return Err("spherical region angular_height must be in (0, pi]");
        }
        // The latitude extent must not run past the poles. A small epsilon
        // tolerates the floating-point representation of ±pi/2 so a region
        // that legitimately reaches a pole is not rejected by rounding.
        const POLE_EPS: f32 = 1e-4;
        let half_h = self.angular_height * 0.5;
        if self.centre_elevation - half_h < -FRAC_PI_2 - POLE_EPS
            || self.centre_elevation + half_h > FRAC_PI_2 + POLE_EPS
        {
            return Err("spherical region latitude extent must stay within [-pi/2, pi/2]");
        }
        Ok(())
    }
}

impl ObjectMeta {
    /// Creates an empty object metadata block with no regions.
    pub fn empty(frame_index: u64) -> Self {
        Self {
            frame_index,
            regions: Vec::new(),
            spherical_regions: Vec::new(),
        }
    }

    /// Validates per-region invariants (bounding boxes well-formed, curve
    /// anchors finite and ordered) and rejects duplicate region IDs.
    ///
    /// Callers that pass `ObjectMeta` to
    /// [`crate::DynamicMeta`] indirectly through the QDRV parse path do
    /// not need to call this; it exists for downstream tooling that
    /// constructs `ObjectMeta` directly (U-13).
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut seen_ids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for region in &self.regions {
            if !seen_ids.insert(region.id) {
                return Err("object region IDs must be unique within a frame");
            }
            region.validate()?;
        }

        // Spherical regions carry an independent ID space and the same
        // endpoint-pinned curve contract as the rectilinear regions above.
        let mut seen_spherical_ids: std::collections::BTreeSet<u32> =
            std::collections::BTreeSet::new();
        for region in &self.spherical_regions {
            if !seen_spherical_ids.insert(region.id) {
                return Err("spherical region IDs must be unique within a frame");
            }
            region.validate()?;
            ToneMapCurve::validate_anchors_shape(&region.tone_map_curve.anchors, true)?;
        }
        Ok(())
    }

    /// Resolves the tone mapping curve for a pixel at the given normalised
    /// position within the frame. Returns the highest-priority region's
    /// curve if the pixel falls within any region, or `None` if it should
    /// use the global frame curve.
    pub fn resolve_curve_at(&self, norm_x: f32, norm_y: f32) -> Option<&ToneMapCurve> {
        self.resolve_curve_at_frame(self.frame_index, norm_x, norm_y)
    }

    /// Resolves the tone mapping curve for a pixel on `frame_index`, applying
    /// bounded rectilinear region motion relative to this metadata block's
    /// authored keyframe.
    pub fn resolve_curve_at_frame(
        &self,
        frame_index: u64,
        norm_x: f32,
        norm_y: f32,
    ) -> Option<&ToneMapCurve> {
        let frame_delta = frame_index.checked_sub(self.frame_index)?;
        self.regions
            .iter()
            .filter(|r| {
                r.bounding_box_at_delta(frame_delta)
                    .is_some_and(|bbox| bbox.contains(norm_x, norm_y))
            })
            .max_by_key(|r| r.priority)
            .map(|r| &r.tone_map_curve)
    }

    /// Returns true if this object metadata block can be applied to
    /// `frame_index` without treating same-frame-only regions as stale.
    pub fn applies_to_frame(&self, frame_index: u64) -> bool {
        if frame_index == self.frame_index {
            return true;
        }
        let Some(frame_delta) = frame_index.checked_sub(self.frame_index) else {
            return false;
        };
        self.spherical_regions.is_empty()
            && !self.regions.is_empty()
            && self
                .regions
                .iter()
                .all(|region| region.bounding_box_at_delta(frame_delta).is_some())
    }

    /// Resolves the tone mapping curve for a spherical coordinate
    /// `(azimuth, elevation)`, both in radians. Returns the highest-priority
    /// [`SphericalRegion`] containing the coordinate, or `None` to fall back
    /// to the global frame curve. This is the spherical counterpart to
    /// [`ObjectMeta::resolve_curve_at`].
    pub fn resolve_spherical_curve_at(
        &self,
        azimuth: f32,
        elevation: f32,
    ) -> Option<&ToneMapCurve> {
        self.spherical_regions
            .iter()
            .filter(|r| r.contains(azimuth, elevation))
            .max_by_key(|r| r.priority)
            .map(|r| &r.tone_map_curve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tone_curve::ToneMapCurve;

    /// Serialises a non-empty `ObjectMeta` to JSON and deserialises it back, expecting bitwise equality of the struct.
    #[test]
    fn test_object_meta_json_roundtrip() {
        let meta = ObjectMeta {
            frame_index: 0,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.1,
                    y: 0.2,
                    width: 0.3,
                    height: 0.4,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 10,
                motion: None,
            }],
            spherical_regions: Vec::new(),
        };
        let json = crate::to_json(&meta).unwrap();
        let recovered: ObjectMeta = crate::from_json(&json).unwrap();
        assert_eq!(meta, recovered);
    }

    /// Validates `ObjectMeta::empty` preserves the frame index and yields an empty region list.
    #[test]
    fn test_empty_object_meta() {
        let meta = ObjectMeta::empty(42);
        assert_eq!(meta.frame_index, 42);
        assert!(meta.regions.is_empty());
        assert!(meta.spherical_regions.is_empty());
    }

    #[test]
    fn object_region_motion_json_roundtrip() {
        let meta = ObjectMeta {
            frame_index: 10,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.1,
                    y: 0.2,
                    width: 0.2,
                    height: 0.2,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 5,
                motion: Some(RegionMotion::Translate {
                    dx_per_frame: 0.05,
                    dy_per_frame: -0.02,
                    frame_count: 4,
                }),
            }],
            spherical_regions: Vec::new(),
        };

        let json = crate::to_json(&meta).unwrap();
        let recovered: ObjectMeta = crate::from_json(&json).unwrap();
        assert_eq!(meta, recovered);
    }

    #[test]
    fn object_region_motion_translate_resolves_on_later_frame() {
        let meta = ObjectMeta {
            frame_index: 4,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.1,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 5,
                motion: Some(RegionMotion::Translate {
                    dx_per_frame: 0.2,
                    dy_per_frame: 0.0,
                    frame_count: 4,
                }),
            }],
            spherical_regions: Vec::new(),
        };

        assert!(meta.resolve_curve_at_frame(6, 0.55, 0.15).is_some());
        assert!(meta.resolve_curve_at_frame(6, 0.15, 0.15).is_none());
        assert!(meta.resolve_curve_at_frame(8, 0.95, 0.15).is_none());
    }

    #[test]
    fn object_region_piecewise_motion_interpolates_between_keyframes() {
        let meta = ObjectMeta {
            frame_index: 10,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.1,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 5,
                motion: Some(RegionMotion::PiecewiseLinear {
                    keyframes: vec![
                        MotionKeyframe {
                            frame_delta: 0,
                            dx: 0.0,
                            dy: 0.0,
                        },
                        MotionKeyframe {
                            frame_delta: 2,
                            dx: 0.4,
                            dy: 0.2,
                        },
                        MotionKeyframe {
                            frame_delta: 4,
                            dx: 0.2,
                            dy: 0.4,
                        },
                    ],
                }),
            }],
            spherical_regions: Vec::new(),
        };

        assert!(meta.validate().is_ok());
        assert!(meta.resolve_curve_at_frame(11, 0.35, 0.25).is_some());
        assert!(meta.resolve_curve_at_frame(11, 0.15, 0.15).is_none());
        assert!(meta.resolve_curve_at_frame(15, 0.35, 0.55).is_none());
    }

    #[test]
    fn object_region_motion_validation_rejects_invalid_paths() {
        let mut meta = ObjectMeta {
            frame_index: 0,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.8,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 5,
                motion: Some(RegionMotion::Translate {
                    dx_per_frame: 0.1,
                    dy_per_frame: 0.0,
                    frame_count: 2,
                }),
            }],
            spherical_regions: Vec::new(),
        };
        assert!(meta.validate().is_err());

        meta.regions[0].bounding_box.x = 0.1;
        meta.regions[0].motion = Some(RegionMotion::Translate {
            dx_per_frame: f32::NAN,
            dy_per_frame: 0.0,
            frame_count: 2,
        });
        assert!(meta.validate().is_err());

        meta.regions[0].motion = Some(RegionMotion::Static { frame_count: 0 });
        assert!(meta.validate().is_err());
    }

    #[test]
    fn object_region_piecewise_motion_validation_rejects_bad_keyframes() {
        let mut meta = ObjectMeta {
            frame_index: 0,
            regions: vec![ObjectRegion {
                id: 1,
                bounding_box: BoundingBox {
                    x: 0.1,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
                tone_map_curve: ToneMapCurve::linear(),
                priority: 5,
                motion: Some(RegionMotion::PiecewiseLinear {
                    keyframes: vec![MotionKeyframe {
                        frame_delta: 0,
                        dx: 0.0,
                        dy: 0.0,
                    }],
                }),
            }],
            spherical_regions: Vec::new(),
        };
        assert!(meta.validate().is_err());

        meta.regions[0].motion = Some(RegionMotion::PiecewiseLinear {
            keyframes: vec![
                MotionKeyframe {
                    frame_delta: 0,
                    dx: 0.1,
                    dy: 0.0,
                },
                MotionKeyframe {
                    frame_delta: 2,
                    dx: 0.2,
                    dy: 0.0,
                },
            ],
        });
        assert!(meta.validate().is_err());

        meta.regions[0].motion = Some(RegionMotion::PiecewiseLinear {
            keyframes: vec![
                MotionKeyframe {
                    frame_delta: 0,
                    dx: 0.0,
                    dy: 0.0,
                },
                MotionKeyframe {
                    frame_delta: 0,
                    dx: 0.2,
                    dy: 0.0,
                },
            ],
        });
        assert!(meta.validate().is_err());
    }

    /// A spherical region centred on the ±π antimeridian must match
    /// coordinates on both sides of the seam and reject the opposite side.
    #[test]
    fn spherical_region_contains_handles_antimeridian_wrap() {
        let region = SphericalRegion {
            id: 1,
            centre_azimuth: PI, // antimeridian
            centre_elevation: 0.0,
            angular_width: 40.0_f32.to_radians(),
            angular_height: 20.0_f32.to_radians(),
            tone_map_curve: ToneMapCurve::linear(),
            priority: 5,
        };
        // +175° and -175° both lie within ±20° of +180°.
        assert!(region.contains(175.0_f32.to_radians(), 0.0));
        assert!(region.contains((-175.0_f32).to_radians(), 0.0));
        // 0° (frame-forward) is on the opposite side of the sphere.
        assert!(!region.contains(0.0, 0.0));
        // Outside the latitude band.
        assert!(!region.contains(PI, 30.0_f32.to_radians()));
    }

    /// `SphericalRegion::validate` accepts an in-range region and rejects
    /// out-of-range angular coordinates.
    #[test]
    fn spherical_region_validate_rejects_out_of_range() {
        let mut region = SphericalRegion {
            id: 1,
            centre_azimuth: 0.0,
            centre_elevation: 0.0,
            angular_width: 1.0,
            angular_height: 1.0,
            tone_map_curve: ToneMapCurve::linear(),
            priority: 0,
        };
        assert!(region.validate().is_ok());
        region.centre_elevation = 2.0; // > π/2
        assert!(region.validate().is_err());
        region.centre_elevation = 0.0;
        region.angular_width = 0.0; // not strictly positive
        assert!(region.validate().is_err());
    }

    /// Overlapping spherical regions resolve to the highest-priority curve;
    /// a coordinate outside every region falls back to `None`.
    #[test]
    fn spherical_resolve_picks_highest_priority() {
        let meta = ObjectMeta {
            frame_index: 0,
            regions: Vec::new(),
            spherical_regions: vec![
                SphericalRegion {
                    id: 1,
                    centre_azimuth: 0.0,
                    centre_elevation: 0.0,
                    angular_width: PI,
                    angular_height: FRAC_PI_2,
                    tone_map_curve: ToneMapCurve::linear(),
                    priority: 1,
                },
                SphericalRegion {
                    id: 2,
                    centre_azimuth: 0.0,
                    centre_elevation: 0.0,
                    angular_width: FRAC_PI_2,
                    angular_height: FRAC_PI_2 / 2.0,
                    tone_map_curve: ToneMapCurve::default_1000nit(),
                    priority: 10,
                },
            ],
        };
        // (0,0) is inside both regions; the priority-10 region wins.
        assert_eq!(
            meta.resolve_spherical_curve_at(0.0, 0.0),
            Some(&ToneMapCurve::default_1000nit())
        );
        // High elevation is outside both regions.
        assert!(meta.resolve_spherical_curve_at(0.0, 1.5).is_none());
    }

    /// Round-trips an `ObjectMeta` carrying a spherical region through JSON.
    #[test]
    fn object_meta_spherical_json_roundtrip() {
        let meta = ObjectMeta {
            frame_index: 3,
            regions: Vec::new(),
            spherical_regions: vec![SphericalRegion {
                id: 7,
                centre_azimuth: 0.5,
                centre_elevation: -0.2,
                angular_width: 0.8,
                angular_height: 0.4,
                tone_map_curve: ToneMapCurve::linear(),
                priority: 4,
            }],
        };
        let json = crate::to_json(&meta).unwrap();
        let recovered: ObjectMeta = crate::from_json(&json).unwrap();
        assert_eq!(meta, recovered);
    }
}
