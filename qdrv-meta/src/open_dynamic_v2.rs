// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Open Dynamic Metadata v2 extensions for QDRV.
//!
//! This module layers scene, object, temporal, and environment-aware policy
//! controls on top of legacy ST 2094-style dynamic metadata.

use serde::{Deserialize, Serialize};

use crate::object_meta::{BoundingBox, SphericalProjection};

/// Display model classes used by the adaptation layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayModelClass {
    /// Self-emissive OLED panel.
    Oled,
    /// Edge/full-array LCD.
    Lcd,
    /// MiniLED local-dimming LCD.
    MiniLed,
    /// Front/rear projector class.
    Projector,
}

/// Scene-level constraints for a frame or shot segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneConstraint {
    /// Human-readable scene tag.
    pub scene_id: String,
    /// Scene start frame index (inclusive).
    pub start_frame: u64,
    /// Scene end frame index (inclusive).
    pub end_frame: u64,
    /// Maximum highlight compression gain allowed for this scene.
    pub max_highlight_compression_gain: f32,
}

/// Object-level creative lock or adaptation constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectConstraint {
    /// Stable object identifier.
    pub object_id: u32,
    /// Object region in normalised coordinates.
    pub region: BoundingBox,
    /// If true, object exposure must remain stable through adaptation.
    pub exposure_locked: bool,
    /// Maximum luminance delta (nits) allowed per frame for this object.
    pub max_luminance_delta_per_frame: f32,
}

/// Temporal anti-pumping and frame-time-aware constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemporalConstraint {
    /// Maximum global tone-map gain delta per frame.
    pub max_global_gain_delta_per_frame: f32,
    /// Anti-pumping smoothing factor in [0, 1].
    pub anti_pumping_strength: f32,
    /// Optional real-time budget used by low-latency gaming paths.
    pub frame_time_budget_ms: Option<f32>,
    /// Optional size of the sliding integration window in frames for multi-frame anti-flicker.
    ///
    /// If configured, the stabiliser tracks recent input frame luminance values over a sliding
    /// window of this length, computing variance-based aggregates to suppress low-frequency pumping
    /// behaviours. If set to `None`, the decoder falls back to a default behaviour of 12 frames.
    #[serde(default)]
    pub integration_window_frames: Option<u8>,
}

impl Default for TemporalConstraint {
    /// Conservative defaults intended to suppress visible "pumping" on
    /// adaptive dynamic-range pipelines without measurably constraining
    /// creative intent:
    ///
    /// - `max_global_gain_delta_per_frame = 0.08` — caps inter-frame global
    ///   tone-map gain changes at roughly 8% per frame. At 60 fps this is
    ///   well below the threshold of visible luminance pulsing for typical
    ///   indoor content while still allowing scene cuts to resolve over
    ///   a few frames.
    /// - `anti_pumping_strength = 0.65` — a moderate IIR-style smoothing
    ///   weight applied to the global gain. Values near 0 disable
    ///   smoothing entirely; values near 1 effectively freeze adaptation.
    ///   0.65 is a starting point that integrates within ~3 frames.
    /// - `frame_time_budget_ms = None` — no real-time budget constraint
    ///   unless the caller is on a gaming/low-latency path that opts in.
    /// - `integration_window_frames = None` — defaults to single-frame anti-pumping
    ///   damping and stabilisation behaviours unless explicitly configured.
    fn default() -> Self {
        Self {
            max_global_gain_delta_per_frame: 0.08,
            anti_pumping_strength: 0.65,
            frame_time_budget_ms: None,
            integration_window_frames: None,
        }
    }
}

/// One local tone-map cell in the spatial grid.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalToneMapCell {
    /// Multiplicative luminance gain.
    pub gain: f32,
    /// Additive offset applied after gain.
    pub offset: f32,
}

impl Default for LocalToneMapCell {
    fn default() -> Self {
        Self {
            gain: 1.0,
            offset: 0.0,
        }
    }
}

/// Spatially varying local tone-map controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalToneMapGrid {
    /// Number of columns.
    pub cols: u16,
    /// Number of rows.
    pub rows: u16,
    /// Row-major cells, length must equal `cols * rows`.
    pub cells: Vec<LocalToneMapCell>,
}

impl LocalToneMapGrid {
    /// Creates an identity grid with `cols × rows` cells.
    pub fn identity(cols: u16, rows: u16) -> Self {
        let count = (cols as usize).saturating_mul(rows as usize);
        Self {
            cols,
            rows,
            cells: vec![LocalToneMapCell::default(); count],
        }
    }

    /// Validates geometry and finite cell values.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.cols == 0 || self.rows == 0 {
            return Err("local tone-map grid requires non-zero cols and rows");
        }
        let expected = (self.cols as usize)
            .checked_mul(self.rows as usize)
            .ok_or("local tone-map grid dimensions overflow usize")?;
        if self.cells.len() != expected {
            return Err("local tone-map grid cell count mismatch");
        }
        for cell in &self.cells {
            if !cell.gain.is_finite() || !cell.offset.is_finite() {
                return Err("local tone-map grid cells must contain finite values");
            }
        }
        Ok(())
    }

    /// Bilinearly samples the grid at normalised coordinates.
    ///
    /// # Preconditions
    ///
    /// **The grid must already be `validate()`-clean.** Callers in the
    /// QDRV pipeline reach this via `DynamicMeta::validate()` →
    /// `OpenDynamicMetadataV2::validate()` →
    /// `LocalToneMapGrid::validate()` at parse time, so by the time
    /// `sample()` runs the geometry and cell-finiteness invariants are
    /// established.
    ///
    /// The previous implementation called `self.validate()` on every
    /// invocation, which dominated frame cost on HD/4K v2 streams (U-5).
    /// To stay defensive in the rare degenerate case where the caller
    /// constructs a grid by hand without validating, the bilinear maths
    /// below still degrades gracefully:
    /// - if either dimension is zero, `sample` returns
    ///   [`LocalToneMapCell::default`];
    /// - clamps on `x0/y0/x1/y1` prevent any out-of-bounds index.
    pub fn sample(&self, nx: f32, ny: f32) -> LocalToneMapCell {
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        // Geometry sanity: a zero-dimension or empty-cell grid yields the
        // identity cell rather than panicking on an out-of-bounds index.
        if cols == 0 || rows == 0 || self.cells.len() != cols * rows {
            return LocalToneMapCell::default();
        }

        let x = nx.clamp(0.0, 1.0) * (cols as f32 - 1.0);
        let y = ny.clamp(0.0, 1.0) * (rows as f32 - 1.0);
        let x0 = (x.floor() as usize).min(cols - 1);
        let y0 = (y.floor() as usize).min(rows - 1);
        let x1 = (x0 + 1).min(cols - 1);
        let y1 = (y0 + 1).min(rows - 1);
        let tx = x - x0 as f32;
        let ty = y - y0 as f32;

        let idx = |cx: usize, cy: usize| cy * cols + cx;
        let c00 = self.cells[idx(x0, y0)];
        let c10 = self.cells[idx(x1, y0)];
        let c01 = self.cells[idx(x0, y1)];
        let c11 = self.cells[idx(x1, y1)];

        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let gain0 = lerp(c00.gain, c10.gain, tx);
        let gain1 = lerp(c01.gain, c11.gain, tx);
        let off0 = lerp(c00.offset, c10.offset, tx);
        let off1 = lerp(c01.offset, c11.offset, tx);

        LocalToneMapCell {
            gain: lerp(gain0, gain1, ty),
            offset: lerp(off0, off1, ty),
        }
    }
}

/// Mastering-to-display adaptation layer descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayAdaptationLayer {
    /// Source mastering peak luminance in nits.
    pub source_mastering_peak_nits: f32,
    /// Abstract display model target peak luminance in nits.
    pub abstract_display_peak_nits: f32,
    /// Target display class.
    pub display_model: DisplayModelClass,
    /// Highlight roll-off strength in [0, 1].
    pub highlight_rolloff_strength: f32,
    /// Shadow lift strength in [0, 1].
    pub shadow_lift_strength: f32,
}

/// SDR-to-HDR reconstruction hints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InverseToneMappingHint {
    /// Strength of highlight detail recovery in [0, 1].
    pub highlight_recovery_strength: f32,
    /// Mid-tone contrast boost in [0, 1].
    pub midtone_contrast_boost: f32,
    /// Colour saturation compensation in [0, 1].
    pub saturation_compensation: f32,
}

impl Default for InverseToneMappingHint {
    /// Mild SDR→HDR reconstruction defaults that approximate the behaviour
    /// of consumer "auto-HDR" boosters without overcooking the result:
    ///
    /// - `highlight_recovery_strength = 0.3` — light highlight extension
    ///   that recovers a moderate amount of detail from clipped/near-clipped
    ///   SDR highlights without introducing hue shifts.
    /// - `midtone_contrast_boost = 0.15` — a subtle S-curve in the
    ///   mid-tones; large enough to add perceived depth, small enough to
    ///   avoid crushing shadows.
    /// - `saturation_compensation = 0.1` — counters the perceived
    ///   desaturation that often accompanies highlight boosting.
    ///
    /// These match the conservative midpoint of the open-implementation
    /// reconstruction described in `qdrv-decode::reconstruct`.
    fn default() -> Self {
        Self {
            highlight_recovery_strength: 0.3,
            midtone_contrast_boost: 0.15,
            saturation_compensation: 0.1,
        }
    }
}

/// Low-latency gaming adaptation profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamingProfile {
    /// Maximum acceptable frame time for this profile.
    pub frame_time_budget_ms: f32,
    /// Additional anti-pumping damping in [0, 1].
    pub anti_pumping_strength: f32,
    /// Maximum tone-map gain delta allowed per frame.
    pub max_gain_delta_per_frame: f32,
}

impl Default for GamingProfile {
    /// Defaults tuned for a 120 fps gaming target with aggressive temporal
    /// stability:
    ///
    /// - `frame_time_budget_ms = 8.3` — 1000 / 120 ≈ 8.33 ms per frame,
    ///   the per-frame budget for sustained 120 fps. Decoders use this as a
    ///   soft deadline for adaptive policy work.
    /// - `anti_pumping_strength = 0.8` — stronger smoothing than the
    ///   broadcast default; gaming HUDs and abrupt camera cuts make
    ///   visible pumping worse, so the IIR is biased toward stability.
    /// - `max_gain_delta_per_frame = 0.05` — tighter than the broadcast
    ///   default's 0.08 to suppress the high-frequency luminance jitter
    ///   that's especially noticeable in fast-motion gameplay.
    fn default() -> Self {
        Self {
            frame_time_budget_ms: 8.3,
            anti_pumping_strength: 0.8,
            max_gain_delta_per_frame: 0.05,
        }
    }
}

/// Ambient-adaptive policy curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbientAdaptivePolicy {
    /// Monotonic ambient lux breakpoints.
    pub lux_breakpoints: Vec<f32>,
    /// Corresponding luminance boost multipliers.
    pub boost_multipliers: Vec<f32>,
    /// Maximum boost delta per second.
    pub max_delta_per_second: f32,
}

impl AmbientAdaptivePolicy {
    /// Samples the policy at the given ambient illuminance.
    ///
    /// # Preconditions
    ///
    /// The policy must already be `validate()`-clean. QDRV's metadata
    /// parse path enforces this transitively via
    /// `OpenDynamicMetadataV2::validate()`. The implementation is still
    /// defensive against degenerate input — empty breakpoint/multiplier
    /// vectors return the no-op multiplier `1.0` instead of panicking.
    pub fn boost_for_lux(&self, lux: f32) -> f32 {
        if self.lux_breakpoints.is_empty() || self.boost_multipliers.is_empty() {
            return 1.0;
        }
        if self.lux_breakpoints.len() != self.boost_multipliers.len() {
            return 1.0;
        }
        if self.lux_breakpoints.len() == 1 {
            return self.boost_multipliers[0];
        }
        let lux = lux.max(0.0);
        for (i, window) in self.lux_breakpoints.windows(2).enumerate() {
            let lo = window[0];
            let hi = window[1];
            if lux >= lo && lux <= hi {
                let span = (hi - lo).max(f32::EPSILON);
                let t = (lux - lo) / span;
                let b0 = self.boost_multipliers[i];
                let b1 = self.boost_multipliers[i + 1];
                return b0 + (b1 - b0) * t;
            }
        }
        if lux < self.lux_breakpoints[0] {
            self.boost_multipliers[0]
        } else {
            *self.boost_multipliers.last().unwrap_or(&1.0)
        }
    }

    /// Validates shape and monotonicity.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.lux_breakpoints.is_empty() || self.boost_multipliers.is_empty() {
            return Err("ambient policy requires at least one breakpoint and boost");
        }
        if self.lux_breakpoints.len() != self.boost_multipliers.len() {
            return Err("ambient policy breakpoint/boost length mismatch");
        }
        for pair in self.lux_breakpoints.windows(2) {
            if pair[0] >= pair[1] {
                return Err("ambient policy lux breakpoints must be strictly increasing");
            }
        }
        for &value in &self.lux_breakpoints {
            if !value.is_finite() || value < 0.0 {
                return Err("ambient policy lux breakpoints must be finite and non-negative");
            }
        }
        for &value in &self.boost_multipliers {
            if !value.is_finite() || value <= 0.0 {
                return Err("ambient policy boost multipliers must be finite and > 0");
            }
        }
        if !self.max_delta_per_second.is_finite() || self.max_delta_per_second <= 0.0 {
            return Err("ambient policy max_delta_per_second must be finite and > 0");
        }
        Ok(())
    }
}

/// Full Open Dynamic Metadata v2 payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenDynamicMetadataV2 {
    /// Scene-level constraints.
    #[serde(default)]
    pub scene_constraints: Vec<SceneConstraint>,
    /// Object-level constraints.
    #[serde(default)]
    pub object_constraints: Vec<ObjectConstraint>,
    /// Temporal controls.
    #[serde(default)]
    pub temporal: TemporalConstraint,
    /// Optional local tone-map grid.
    #[serde(default)]
    pub local_tone_map_grid: Option<LocalToneMapGrid>,
    /// Optional display adaptation layer.
    #[serde(default)]
    pub adaptation_layer: Option<DisplayAdaptationLayer>,
    /// Optional ambient policy.
    #[serde(default)]
    pub ambient_policy: Option<AmbientAdaptivePolicy>,
    /// Optional low-latency gaming profile.
    #[serde(default)]
    pub gaming_profile: Option<GamingProfile>,
    /// Optional inverse mapping hints for SDR->HDR reconstruction.
    #[serde(default)]
    pub inverse_tone_mapping_hint: Option<InverseToneMappingHint>,
    /// Optional 360°/immersive projection. When present, the per-frame
    /// [`crate::object_meta::SphericalRegion`] entries are interpreted under
    /// this projection; when absent, the stream is treated as flat.
    #[serde(default)]
    pub spherical_projection: Option<SphericalProjection>,
}

impl OpenDynamicMetadataV2 {
    /// Validates all optional and required v2 substructures.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.temporal.max_global_gain_delta_per_frame.is_finite()
            || self.temporal.max_global_gain_delta_per_frame < 0.0
        {
            return Err("temporal max_global_gain_delta_per_frame must be finite and >= 0");
        }
        if !self.temporal.anti_pumping_strength.is_finite()
            || !(0.0..=1.0).contains(&self.temporal.anti_pumping_strength)
        {
            return Err("temporal anti_pumping_strength must be in [0, 1]");
        }
        if let Some(v) = self.temporal.frame_time_budget_ms
            && (!v.is_finite() || v <= 0.0)
        {
            return Err("temporal frame_time_budget_ms must be finite and > 0");
        }
        // Enforce that the sliding integration window frame size must be strictly positive
        // to prevent division-by-zero errors when normalising the mean and variance aggregates.
        if let Some(w) = self.temporal.integration_window_frames
            && w == 0
        {
            return Err("temporal integration_window_frames must be > 0");
        }

        // Track seen scene IDs so duplicate IDs are rejected (U-8). Downstream
        // decoders that assume unique IDs (e.g., keyed lookup, per-scene
        // statistics) cannot recover from a collision once the metadata is
        // committed, so we surface it here.
        let mut seen_scene_ids: std::collections::BTreeSet<&str> =
            std::collections::BTreeSet::new();
        for scene in &self.scene_constraints {
            if scene.scene_id.trim().is_empty() {
                return Err("scene constraint scene_id must not be empty");
            }
            if !seen_scene_ids.insert(scene.scene_id.as_str()) {
                return Err("scene constraint scene_id must be unique");
            }
            if scene.end_frame < scene.start_frame {
                return Err("scene constraint end_frame must be >= start_frame");
            }
            if !scene.max_highlight_compression_gain.is_finite()
                || scene.max_highlight_compression_gain <= 0.0
            {
                return Err("scene max_highlight_compression_gain must be finite and > 0");
            }
        }

        // Same uniqueness requirement for object IDs (U-8).
        let mut seen_object_ids: std::collections::BTreeSet<u32> =
            std::collections::BTreeSet::new();
        for object in &self.object_constraints {
            if !seen_object_ids.insert(object.object_id) {
                return Err("object constraint object_id must be unique");
            }
            if !object.max_luminance_delta_per_frame.is_finite()
                || object.max_luminance_delta_per_frame < 0.0
            {
                return Err("object max_luminance_delta_per_frame must be finite and >= 0");
            }
            // Delegate bounding-box invariants to the single authoritative
            // implementation in `object_meta::BoundingBox::validate()` so
            // both v2 object constraints and standalone `ObjectMeta`
            // regions enforce identical geometry rules. Previously this
            // block hand-inlined an independent copy of the same checks
            // (audit finding I-1).
            object.region.validate()?;
        }

        if let Some(grid) = &self.local_tone_map_grid {
            grid.validate()?;
        }
        if let Some(layer) = &self.adaptation_layer {
            if !layer.source_mastering_peak_nits.is_finite()
                || !layer.abstract_display_peak_nits.is_finite()
                || layer.source_mastering_peak_nits <= 0.0
                || layer.abstract_display_peak_nits <= 0.0
            {
                return Err("adaptation layer peaks must be finite and > 0");
            }
            if !layer.highlight_rolloff_strength.is_finite()
                || !(0.0..=1.0).contains(&layer.highlight_rolloff_strength)
            {
                return Err("adaptation highlight_rolloff_strength must be in [0,1]");
            }
            if !layer.shadow_lift_strength.is_finite()
                || !(0.0..=1.0).contains(&layer.shadow_lift_strength)
            {
                return Err("adaptation shadow_lift_strength must be in [0,1]");
            }
        }
        if let Some(policy) = &self.ambient_policy {
            policy.validate()?;
        }
        if let Some(gaming) = &self.gaming_profile {
            if !gaming.frame_time_budget_ms.is_finite() || gaming.frame_time_budget_ms <= 0.0 {
                return Err("gaming frame_time_budget_ms must be finite and > 0");
            }
            if !gaming.anti_pumping_strength.is_finite()
                || !(0.0..=1.0).contains(&gaming.anti_pumping_strength)
            {
                return Err("gaming anti_pumping_strength must be in [0, 1]");
            }
            if !gaming.max_gain_delta_per_frame.is_finite() || gaming.max_gain_delta_per_frame < 0.0
            {
                return Err("gaming max_gain_delta_per_frame must be finite and >= 0");
            }
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
                return Err("inverse tone mapping hint values must be in [0, 1]");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_sampling_returns_identity_for_uniform_grid() {
        let grid = LocalToneMapGrid::identity(2, 2);
        let sample = grid.sample(0.4, 0.7);
        assert!((sample.gain - 1.0).abs() < 1e-6);
        assert!(sample.offset.abs() < 1e-6);
    }

    #[test]
    fn ambient_policy_interpolates() {
        let policy = AmbientAdaptivePolicy {
            lux_breakpoints: vec![0.0, 100.0, 500.0],
            boost_multipliers: vec![1.0, 1.1, 1.3],
            max_delta_per_second: 0.5,
        };
        let boost = policy.boost_for_lux(300.0);
        assert!(boost > 1.1 && boost < 1.3);
    }

    #[test]
    fn v2_validate_rejects_bad_scene_bounds() {
        let meta = OpenDynamicMetadataV2 {
            scene_constraints: vec![SceneConstraint {
                scene_id: "bad".to_string(),
                start_frame: 10,
                end_frame: 9,
                max_highlight_compression_gain: 1.0,
            }],
            object_constraints: Vec::new(),
            temporal: TemporalConstraint::default(),
            local_tone_map_grid: None,
            adaptation_layer: None,
            ambient_policy: None,
            gaming_profile: None,
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        };
        assert!(meta.validate().is_err());
    }

    #[test]
    fn v2_validate_rejects_bad_temporal_window() {
        let meta = OpenDynamicMetadataV2 {
            scene_constraints: Vec::new(),
            object_constraints: Vec::new(),
            temporal: TemporalConstraint {
                integration_window_frames: Some(0),
                ..TemporalConstraint::default()
            },
            local_tone_map_grid: None,
            adaptation_layer: None,
            ambient_policy: None,
            gaming_profile: None,
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        };
        assert!(meta.validate().is_err());
    }
}
