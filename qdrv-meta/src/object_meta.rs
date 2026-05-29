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
//! This extension is compatible with the SMPTE ST 2094 framework, which
//! reserves provisions for region-based processing.

use crate::tone_curve::ToneMapCurve;
use serde::{Deserialize, Serialize};

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

impl ObjectMeta {
    /// Creates an empty object metadata block with no regions.
    pub fn empty(frame_index: u64) -> Self {
        Self {
            frame_index,
            regions: Vec::new(),
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
            region.bounding_box.validate()?;
            // Delegate anchor invariants to the shared validator on
            // `ToneMapCurve`. Per-region curves must pin endpoints at 0/1
            // for the same reason the global per-frame curve does — the
            // tone-mapper at any pixel falling inside this region has to
            // be able to evaluate the curve across the full PQ range.
            // Audit finding J-1 consolidation.
            ToneMapCurve::validate_anchors_shape(&region.tone_map_curve.anchors, true)?;
        }
        Ok(())
    }

    /// Resolves the tone mapping curve for a pixel at the given normalised
    /// position within the frame. Returns the highest-priority region's
    /// curve if the pixel falls within any region, or `None` if it should
    /// use the global frame curve.
    pub fn resolve_curve_at(&self, norm_x: f32, norm_y: f32) -> Option<&ToneMapCurve> {
        self.regions
            .iter()
            .filter(|r| r.bounding_box.contains(norm_x, norm_y))
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
            }],
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
    }
}
