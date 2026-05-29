// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Per-frame dynamic metadata for QDRV delivery-tier streams.
//!
//! Dynamic metadata is the mechanism by which a colourist's per-scene or
//! per-frame tone mapping intent is communicated to a decoder. It is based on
//! the SMPTE ST 2094 dynamic metadata framework, with all numeric values stored
//! as IEEE 754 Float32 rather than the integer representations used by
//! ST 2094-40 (HDR10+) and ST 2094-10 (Dolby Vision).
//!
//! One [`DynamicMeta`] block is carried per frame in the delivery-tier stream.

use crate::tone_curve::ToneMapCurve;
use crate::{
    compatibility::{METADATA_SCHEMA_V1, METADATA_SCHEMA_V2},
    open_dynamic_v2::{InverseToneMappingHint, OpenDynamicMetadataV2},
};
use serde::{Deserialize, Serialize};

/// Target display capability hint embedded in per-frame dynamic metadata.
///
/// Communicates the reference display for which the tone mapping curve in this
/// frame was authored. Decoders use this information to adapt the curve to the
/// actual target display's peak luminance and black level. This is analogous to
/// the display capability signalling defined in SMPTE ST 2094, extended to
/// Float32 precision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayHint {
    /// Minimum (black level) luminance of the reference display, in nits.
    pub min_luminance_nits: f32,
    /// Maximum (peak white) luminance of the reference display, in nits.
    pub max_luminance_nits: f32,
}

impl Default for DisplayHint {
    /// Returns the QDRV reference display specification:
    /// 1 000 nits peak luminance, 0.0005 nits black level.
    ///
    /// This matches the current professional HDR reference monitor standard
    /// and is consistent with the HDR10 and Dolby Vision reference display
    /// definitions.
    fn default() -> Self {
        Self {
            min_luminance_nits: 0.0005,
            max_luminance_nits: 1_000.0,
        }
    }
}

/// Per-frame dynamic metadata for a QDRV delivery-tier stream.
///
/// One instance of this structure is carried per frame. The scene luminance
/// statistics (`scene_peak_luminance_nits` and `scene_average_luminance_nits`)
/// allow decoders to perform scene-level adaptation, while the
/// `tone_map_curve` carries the colourist's per-frame creative intent as a
/// set of Bézier anchor points.
///
/// This structure is a floating-point extension of the SMPTE ST 2094 dynamic
/// metadata model. Where ST 2094-40 (HDR10+) stores luminance statistics and
/// tone curve parameters as 12-bit or 16-bit integers, QDRV stores them as
/// IEEE 754 Float32, eliminating quantisation error in the metadata itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicMeta {
    /// Metadata schema version.
    #[serde(default = "default_metadata_schema_version")]
    pub metadata_schema_version: u16,
    /// Zero-based index of this frame within the stream.
    pub frame_index: u64,
    /// Peak luminance of the scene containing this frame, in nits.
    pub scene_peak_luminance_nits: f32,
    /// Average luminance of the scene containing this frame, in nits.
    pub scene_average_luminance_nits: f32,
    /// Per-frame tone mapping curve authored for the reference display.
    pub tone_map_curve: ToneMapCurve,
    /// Reference display characteristics for which this frame's tone curve
    /// was authored. Decoders adapt the curve from this hint to the actual
    /// target display at runtime.
    pub target_display_hint: DisplayHint,
    /// Open Dynamic Metadata v2 payload.
    #[serde(default)]
    pub open_dynamic_v2: Option<OpenDynamicMetadataV2>,
    /// Bidirectional SDR->HDR reconstruction hints.
    #[serde(default)]
    pub inverse_tone_mapping_hint: Option<InverseToneMappingHint>,
    /// If true, non-authorial adaptation policies must be bypassed.
    #[serde(default)]
    pub creator_intent_locked: bool,
}

impl DynamicMeta {
    /// Creates a per-frame metadata block for the given frame index and scene
    /// luminance statistics. The tone mapping curve is initialised to the
    /// default 1 000-nit Bézier curve, and the display hint is set to the
    /// QDRV reference display (1 000 nits peak, 0.0005 nits black).
    ///
    /// # Arguments
    /// * `frame_index` — Zero-based frame index within the stream.
    /// * `scene_peak`  — Peak luminance of the scene, in nits.
    /// * `scene_avg`   — Average luminance of the scene, in nits.
    pub fn new(frame_index: u64, scene_peak: f32, scene_avg: f32) -> Self {
        Self {
            metadata_schema_version: METADATA_SCHEMA_V1,
            frame_index,
            scene_peak_luminance_nits: scene_peak,
            scene_average_luminance_nits: scene_avg,
            tone_map_curve: ToneMapCurve::default_1000nit(),
            target_display_hint: DisplayHint::default(),
            open_dynamic_v2: None,
            inverse_tone_mapping_hint: None,
            creator_intent_locked: false,
        }
    }

    /// Validates cross-field invariants required by QDRV metadata.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.metadata_schema_version < METADATA_SCHEMA_V1
            || self.metadata_schema_version > METADATA_SCHEMA_V2
        {
            return Err("metadata_schema_version is unsupported");
        }

        let peak = self.scene_peak_luminance_nits;
        let avg = self.scene_average_luminance_nits;
        if !peak.is_finite() || !avg.is_finite() {
            return Err("scene luminance values must be finite");
        }
        if peak < 0.0 || avg < 0.0 {
            return Err("scene luminance values must be non-negative");
        }
        if avg > peak {
            return Err("scene_average_luminance_nits exceeds scene_peak_luminance_nits");
        }

        let hint = &self.target_display_hint;
        if !hint.min_luminance_nits.is_finite() || !hint.max_luminance_nits.is_finite() {
            return Err("display hint luminance values must be finite");
        }
        if hint.min_luminance_nits < 0.0 {
            return Err("display hint min luminance must be non-negative");
        }
        if hint.max_luminance_nits <= hint.min_luminance_nits {
            return Err("display hint requires max_luminance_nits > min_luminance_nits");
        }

        // Delegate the shared anchor-shape invariants to the single
        // authoritative validator on `ToneMapCurve`. The previous inlined
        // copy here drifted independently from the same checks in
        // `ToneMapCurve::from_anchors` and `ObjectMeta::validate` (audit
        // finding J-1); the consolidation keeps all three sites in lock
        // step. `endpoints_required = true` because `DynamicMeta` carries
        // the per-frame *global* tone curve and the QDRV format requires it
        // to span `[0.0, 1.0]` end-to-end.
        ToneMapCurve::validate_anchors_shape(&self.tone_map_curve.anchors, true)?;

        if let Some(v2) = &self.open_dynamic_v2 {
            v2.validate()?;
        }

        if let Some(hint) = &self.inverse_tone_mapping_hint {
            let values = [
                hint.highlight_recovery_strength,
                hint.midtone_contrast_boost,
                hint.saturation_compensation,
            ];
            if values
                .iter()
                .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            {
                return Err("inverse_tone_mapping_hint values must be in [0, 1]");
            }
        }
        Ok(())
    }
}

fn default_metadata_schema_version() -> u16 {
    METADATA_SCHEMA_V1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_accepts_default_metadata() {
        let meta = DynamicMeta::new(0, 1000.0, 200.0);
        assert!(meta.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_scene_average_above_peak() {
        let meta = DynamicMeta::new(0, 100.0, 200.0);
        assert!(meta.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_unsorted_curve_inputs() {
        let mut meta = DynamicMeta::new(0, 1000.0, 200.0);
        meta.tone_map_curve.anchors.swap(1, 2);
        assert!(meta.validate().is_err());
    }
}
