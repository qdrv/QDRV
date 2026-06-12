// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Display-adaptive tone mapping for QDRV delivery-tier streams.
//!
//! Applies the per-frame SMPTE ST 2094-based tone mapping curve from QDRV
//! dynamic metadata to a buffer of delivery-tier pixels, adapting the
//! colourist's creative intent from the reference display to the actual
//! target display at runtime.
//!
//! ## Tone mapping pipeline (per pixel, per channel)
//!
//! 1. Decode the ST 2084 PQ signal to linear light in nits (EOTF).
//! 2. Normalise the linear nit value to `[0.0, 1.0]` relative to the
//!    reference display's peak luminance.
//! 3. Evaluate the per-frame tone mapping curve at the normalised value.
//! 4. Scale the mapped value to the target display's luminance range.
//! 5. Re-encode the scaled value to the ST 2084 PQ signal (OETF).
//!
//! All arithmetic is performed in Float32 as required for the delivery tier.

use qdrv_core::{
    pixel::Pixel32,
    pq::{PQ_MAX_NITS, pq_eotf_f32, pq_oetf_f32},
};
use qdrv_meta::{DisplayHint, DynamicMeta, ToneMapCurve, open_dynamic_v2::DisplayModelClass};

/// The luminance capabilities of a target display, used to adapt the QDRV
/// tone mapping curve from the reference display to the actual output device.
#[derive(Debug, Clone, Copy)]
pub struct TargetDisplay {
    /// Black level (minimum luminance) of the target display, in nits.
    pub min_nits: f32,
    /// Peak white (maximum luminance) of the target display, in nits.
    pub max_nits: f32,
}

impl Default for TargetDisplay {
    /// Returns the QDRV reference display:
    /// 1 000 nits peak luminance, 0.0005 nits black level.
    fn default() -> Self {
        Self {
            min_nits: 0.0005,
            max_nits: 1_000.0,
        }
    }
}

impl From<DisplayHint> for TargetDisplay {
    /// Creates a `TargetDisplay` from the display hint embedded in per-frame
    /// QDRV dynamic metadata.
    fn from(hint: DisplayHint) -> Self {
        Self {
            min_nits: hint.min_luminance_nits,
            max_nits: hint.max_luminance_nits,
        }
    }
}

/// Render execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Standard adaptive mode.
    Standard,
    /// Deterministic mode with quantised intermediate states for reproducible
    /// frame outputs across identical runs.
    Deterministic,
}

/// Optional runtime render policy controls.
#[derive(Debug, Clone, Copy)]
pub struct RenderPolicy {
    /// Determinism mode.
    pub mode: RenderMode,
    /// Current ambient lux for ambient-adaptive policies.
    pub ambient_lux: Option<f32>,
    /// Current frame-time estimate in milliseconds for gaming profile adaptation.
    pub frame_time_ms: Option<f32>,
    /// Target display model class for adaptation policy.
    pub display_model: Option<DisplayModelClass>,
    /// If true, creator intent lock suppresses non-authorial adaptation.
    pub respect_creator_intent_lock: bool,
}

impl Default for RenderPolicy {
    fn default() -> Self {
        Self {
            mode: RenderMode::Standard,
            ambient_lux: None,
            frame_time_ms: None,
            display_model: None,
            respect_creator_intent_lock: true,
        }
    }
}

/// Stateful temporal controller for frame-sequence anti-pumping.
///
/// Keeps track of the rendering state across a sequence of frames to apply
/// temporal stabilisation. This prevents visible luminance "pumping" or flickering
/// behaviours when adapting colour curves dynamically.
///
/// Callers processing consecutive frame sequences should retain a single instance of
/// this manager per stream and pass a mutable reference to it across successive
/// tone-mapping operations.
#[derive(Debug, Clone)]
pub struct TemporalStateManager {
    /// The global gain factor applied to the previous frame's luminance.
    last_global_gain: f32,
    /// The average luminance of the previous output frame after gain correction.
    last_frame_luma: Option<f32>,
    /// A sliding ring-buffer tracking the average input luminance of recent frames.
    /// This buffer is utilised to compute running statistical aggregates.
    luma_history: std::collections::VecDeque<f32>,
    /// The computed statistical mean of average input luminance values within the current sliding window.
    running_mean: f32,
    /// The computed statistical variance of average input luminance values within the current sliding window.
    running_variance: f32,
}

impl Default for TemporalStateManager {
    fn default() -> Self {
        Self {
            last_global_gain: 1.0,
            last_frame_luma: None,
            luma_history: std::collections::VecDeque::new(),
            running_mean: 0.0,
            running_variance: 0.0,
        }
    }
}

impl TemporalStateManager {
    /// Resets temporal state to its initial neutral values.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Stabilises the global gain factor for the current frame by applying temporal constraint rules.
    ///
    /// This method enforces frame-to-frame gain delta limits and applies smoothing behaviours.
    /// If an integration window is configured and has collected sufficient history (at least two frames),
    /// the controller calculates the running standard deviation over the window. When this standard
    /// deviation is below the stability threshold (`STABLE_LUMA_EPSILON`), the controller identifies the
    /// shot as temporally stable. Under stable conditions, gain adjustments are damped proportionally
    /// to the amount of variance in order to actively suppress slow, low-frequency luminance pumping.
    ///
    /// If the sliding window is disabled or has insufficient history, the algorithm falls back to a simpler,
    /// single-frame step difference dampening logic.
    ///
    /// Once the stabilised gain is computed, the input average luminance of the current frame is appended
    /// to the ring-buffer, and the sliding aggregates (mean and variance) are updated.
    fn stabilize_gain(
        &mut self,
        requested_gain: f32,
        frame_luma: f32,
        max_delta: f32,
        anti_pumping_strength: f32,
        integration_window: usize,
    ) -> f32 {
        // Audit finding S-5: the following constants were previously inline
        // magic numbers. Documenting their role here keeps the production
        // control path inspectable.
        //
        // - `STRENGTH_TO_SMOOTHING_SCALE` (0.85): Caps the maximum
        //   effective smoothing weight at 0.85, leaving 15% headroom so a
        //   `strength = 1.0` policy still admits some new-frame influence
        //   instead of locking the gain completely.
        // - `MIN_SMOOTHING` (0.05): Floor that prevents the smoothing
        //   weight from collapsing to zero and producing single-frame
        //   pumping under near-maximum strength.
        // - `MIN_ALLOWED_DELTA` (0.000_5): Floor on the per-frame allowed
        //   gain delta so a metadata-supplied `max_gain_delta_per_frame`
        //   of exactly 0 cannot freeze adaptation entirely.
        // - `MIN_STABILIZED_GAIN` (0.35): Hard floor on the stabilised
        //   gain. Below this value highlights are crushed to a degree the
        //   tone curve cannot recover from; the floor protects against
        //   pathological adaptation requests.
        // - `STABLE_LUMA_EPSILON` (0.015): If frame-average luma changes
        //   by less than this threshold, the scene is "stable" and we
        //   damp large gain swings extra-hard (see below).
        // - `STABLE_LUMA_DAMP` (0.5): When a stable scene asks for a
        //   gain swing larger than the per-frame budget, blend back
        //   toward the previous gain by 50% rather than letting the
        //   max-delta clamp alone govern the move.
        const STRENGTH_TO_SMOOTHING_SCALE: f32 = 0.85;
        const MIN_SMOOTHING: f32 = 0.05;
        const MIN_ALLOWED_DELTA: f32 = 0.000_5;
        const MIN_STABILIZED_GAIN: f32 = 0.35;
        const STABLE_LUMA_EPSILON: f32 = 0.015;
        const STABLE_LUMA_DAMP: f32 = 0.5;

        let prev_gain = self.last_global_gain;
        let smoothing = (1.0 - anti_pumping_strength.clamp(0.0, 1.0) * STRENGTH_TO_SMOOTHING_SCALE)
            .clamp(MIN_SMOOTHING, 1.0);
        let mut smoothed = prev_gain + (requested_gain - prev_gain) * smoothing;

        let allowed_delta = max_delta.max(MIN_ALLOWED_DELTA);
        smoothed = smoothed
            .clamp(prev_gain - allowed_delta, prev_gain + allowed_delta)
            .clamp(MIN_STABILIZED_GAIN, 1.0);

        // Apply multi-frame integration damping if the history is stable.
        // If the running standard deviation over the sliding window is below
        // the stable threshold, the scene is static and we damp gain changes
        // proportionally. Otherwise, we fall back to standard single-frame damping.
        if integration_window > 0 && self.luma_history.len() >= 2 {
            let std_dev = self.running_variance.sqrt();
            if std_dev < STABLE_LUMA_EPSILON {
                // Blend delta back toward prev_gain proportionally based on the stability of the window.
                let damp_factor =
                    1.0 - (1.0 - STABLE_LUMA_DAMP) * (1.0 - std_dev / STABLE_LUMA_EPSILON);
                smoothed = prev_gain + (smoothed - prev_gain) * damp_factor;
            }
        } else if let Some(prev_luma) = self.last_frame_luma {
            // Fall back to single-frame stability damping if history is insufficient.
            let luma_delta = (frame_luma - prev_luma).abs();
            if luma_delta < STABLE_LUMA_EPSILON
                && (requested_gain - prev_gain).abs() > allowed_delta
            {
                smoothed = prev_gain + (smoothed - prev_gain) * STABLE_LUMA_DAMP;
            }
        }

        self.last_global_gain = smoothed;
        let output_luma = frame_luma * smoothed;
        self.last_frame_luma = Some(output_luma);

        // Update the ring-buffer with the input luminance of the current frame.
        if integration_window > 0 {
            while self.luma_history.len() >= integration_window {
                self.luma_history.pop_front();
            }
            self.luma_history.push_back(frame_luma);

            let n = self.luma_history.len() as f32;
            let mean = self.luma_history.iter().sum::<f32>() / n;
            let variance = self
                .luma_history
                .iter()
                .map(|&val| {
                    let diff = val - mean;
                    diff * diff
                })
                .sum::<f32>()
                / n;

            self.running_mean = mean;
            self.running_variance = variance;
        }

        smoothed
    }
}

/// Applies display-adaptive tone mapping to a buffer of QDRV delivery-tier pixels.
///
/// Adapts the per-frame SMPTE ST 2094-based tone mapping curve from the
/// dynamic metadata to the target display's actual capabilities, following
/// the five-step pipeline described in the [module documentation](self).
///
/// # Arguments
/// * `pixels`  — Delivery-tier PQ-encoded pixels in row-major order.
/// * `width`   — Frame width in pixels (used for 2-D local-tone-map sampling).
/// * `height`  — Frame height in pixels.
/// * `dynamic` — Per-frame dynamic metadata carrying the tone curve and scene
///   luminance statistics.
/// * `target`  — Capabilities of the target display.
///
/// # Returns
/// A new `Vec<Pixel32>` containing the tone-mapped, PQ re-encoded output pixels.
pub fn tone_map_frame(
    pixels: &[Pixel32],
    width: u32,
    height: u32,
    dynamic: &DynamicMeta,
    target: &TargetDisplay,
) -> Vec<Pixel32> {
    tone_map_frame_with_policy(
        pixels,
        width,
        height,
        dynamic,
        target,
        &RenderPolicy::default(),
    )
}

/// Applies display-adaptive tone mapping with explicit runtime policy controls.
pub fn tone_map_frame_with_policy(
    pixels: &[Pixel32],
    width: u32,
    height: u32,
    dynamic: &DynamicMeta,
    target: &TargetDisplay,
    policy: &RenderPolicy,
) -> Vec<Pixel32> {
    let mut state = TemporalStateManager::default();
    tone_map_frame_with_policy_and_state(pixels, width, height, dynamic, target, policy, &mut state)
}

/// Applies display-adaptive tone mapping with explicit policy and temporal state.
///
/// Callers that process frame sequences should retain `state` across frame
/// boundaries to enable stateful anti-pumping behaviour.
///
/// `width`/`height` are required for true 2-D sampling of any
/// `local_tone_map_grid` carried in v2 dynamic metadata. Without them, the
/// per-pixel `(norm_x, norm_y)` coordinate would degenerate to a 1-D ramp
/// over flat-pixel-index, which is the audit defect addressed by this
/// signature change.
pub fn tone_map_frame_with_policy_and_state(
    pixels: &[Pixel32],
    width: u32,
    height: u32,
    dynamic: &DynamicMeta,
    target: &TargetDisplay,
    policy: &RenderPolicy,
    state: &mut TemporalStateManager,
) -> Vec<Pixel32> {
    let ref_max = dynamic.target_display_hint.max_luminance_nits;
    let (target_min_nits, target_max_nits) = sanitise_target_range(target);
    let creator_lock_active = dynamic.creator_intent_locked && policy.respect_creator_intent_lock;
    let w = width as usize;
    let h = height as usize;
    // Pre-compute the row/col → normalised-coordinate scale factors. For
    // degenerate single-pixel rows/cols the divisor is at least 1 so we
    // place that sample at norm coord 0.0 instead of dividing by zero.
    let inv_w = if w == 0 { 0.0_f32 } else { 1.0 / w as f32 };
    let inv_h = if h == 0 { 0.0_f32 } else { 1.0 / h as f32 };

    // Frame-level state bundled once so the per-pixel inner loop only
    // passes the per-pixel coordinates (audit L-05 refactor).
    let ctx = MapChannelCtx {
        curve: &dynamic.tone_map_curve,
        ref_max,
        target_min_nits,
        target_max_nits,
        dynamic,
        policy,
        creator_lock_active,
    };

    let mut mapped = pixels
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            // Row-major raster → spatial normalised coordinates in `[0, 1)`.
            // When width/height are zero (callers should not do this, but we
            // tolerate it defensively) both axes collapse to 0.0.
            let (norm_x, norm_y) = if w == 0 {
                (0.0_f32, 0.0_f32)
            } else {
                let col = (idx % w) as f32 * inv_w;
                let row = (idx / w) as f32 * inv_h;
                (col.clamp(0.0, 1.0), row.clamp(0.0, 1.0))
            };
            let r = map_channel(p.r, &ctx, norm_x, norm_y);
            let g = map_channel(p.g, &ctx, norm_x, norm_y);
            let b = map_channel(p.b, &ctx, norm_x, norm_y);
            safe_pixel32(r, g, b)
        })
        .collect::<Vec<_>>();

    apply_temporal_gaming_controller(&mut mapped, dynamic, policy, creator_lock_active, state);
    mapped
}

/// Per-frame state passed into [`map_channel`].
///
/// Audit L-05 refactor: previously `map_channel` took 10 positional
/// arguments. The frame-level invariants (curve, target display range,
/// dynamic metadata, render policy, creator-lock flag) are now grouped
/// into [`MapChannelCtx`] which is built once per `tone_map_frame_*`
/// call. Only the per-pixel inputs (`pq_in`, `norm_x`, `norm_y`) remain
/// positional.
struct MapChannelCtx<'a> {
    curve: &'a ToneMapCurve,
    ref_max: f32,
    target_min_nits: f32,
    target_max_nits: f32,
    dynamic: &'a DynamicMeta,
    policy: &'a RenderPolicy,
    creator_lock_active: bool,
}

/// Applies the full tone mapping pipeline to a single PQ-encoded channel value.
///
/// # Arguments
/// * `pq_in`  — Input PQ-encoded channel value in `[0.0, 1.0]`.
/// * `ctx`    — Frame-level state (curve, target range, dynamic meta, policy).
/// * `norm_x` / `norm_y` — Per-pixel normalised coordinates for v2 grid sampling.
#[inline]
fn map_channel(pq_in: f32, ctx: &MapChannelCtx<'_>, norm_x: f32, norm_y: f32) -> f32 {
    let curve = ctx.curve;
    let ref_max = ctx.ref_max;
    let target_min_nits = ctx.target_min_nits;
    let target_max_nits = ctx.target_max_nits;
    let dynamic = ctx.dynamic;
    let policy = ctx.policy;
    let creator_lock_active = ctx.creator_lock_active;
    // Step 1: Decode the PQ signal to absolute luminance in nits.
    let linear_nits = pq_eotf_f32(pq_in) * PQ_MAX_NITS as f32;

    // Step 2: Normalise to the reference display range [0.0, 1.0].
    // Values above the reference display's peak are clamped, ensuring that
    // the tone mapping curve operates on a well-defined input range.
    let normalised = (linear_nits / ref_max.max(1.0)).clamp(0.0, 1.0);

    // Step 3: Evaluate the per-frame tone mapping curve.
    let mut mapped = curve.evaluate(normalised);

    if !creator_lock_active && let Some(v2) = &dynamic.open_dynamic_v2 {
        if let Some(grid) = &v2.local_tone_map_grid {
            let sample = grid.sample(norm_x, norm_y);
            mapped = (mapped * sample.gain + sample.offset).clamp(0.0, 1.0);
        }
        if let Some(layer) = &v2.adaptation_layer {
            mapped = apply_display_model_adaptation(mapped, layer, policy.display_model);
        }
        if let (Some(ambient), Some(policy_cfg)) = (policy.ambient_lux, &v2.ambient_policy) {
            mapped = (mapped * policy_cfg.boost_for_lux(ambient)).clamp(0.0, 1.0);
        }
    }

    if matches!(policy.mode, RenderMode::Deterministic) {
        // Snap to a fixed grid to provide bit-identical behaviour across runs.
        mapped = ((mapped * 65_535.0).round() / 65_535.0).clamp(0.0, 1.0);
    }

    // Step 4: Scale to the target display's luminance range.
    let output_nits = (mapped * target_max_nits).clamp(target_min_nits, target_max_nits);

    // Step 5: Re-encode the output luminance to the ST 2084 PQ signal.
    pq_oetf_f32(output_nits / PQ_MAX_NITS as f32)
}

fn apply_temporal_gaming_controller(
    pixels: &mut [Pixel32],
    dynamic: &DynamicMeta,
    policy: &RenderPolicy,
    creator_lock_active: bool,
    state: &mut TemporalStateManager,
) {
    if pixels.is_empty() || creator_lock_active {
        return;
    }

    let Some(v2) = &dynamic.open_dynamic_v2 else {
        return;
    };
    let Some(gaming) = &v2.gaming_profile else {
        return;
    };
    let Some(frame_time_ms) = policy.frame_time_ms else {
        return;
    };

    // Audit finding S-5: explain the previously-inline magic numbers used
    // by the gaming-mode temporal controller.
    //
    // - `MIN_FRAME_BUDGET_MS` (0.1 ms): guard against a metadata-supplied
    //   budget of exactly 0 producing a divide-by-zero overload ratio.
    //   0.1 ms is below any plausible real-world frame budget (~1 / 10 000
    //   fps) but keeps the ratio well-defined.
    // - `OVERLOAD_CLAMP_LO` (0.5): the smallest overload ratio we credit.
    //   A renderer reporting that it's running at *twice* the budget speed
    //   doesn't get a brighter-than-reference reward; we cap at "comfortably
    //   under budget" so the controller stays neutral on fast hardware.
    // - `OVERLOAD_CLAMP_HI` (3.0): the largest overload ratio we credit.
    //   At 3× over budget the requested gain has already cratered to its
    //   floor; clamping prevents further roll-off from being requested
    //   when it would otherwise just deepen the floor saturation.
    // - `BUDGET_OVERRUN_DAMP` (0.35): proportional scaling on
    //   `anti_pumping_strength` for budget overruns. Combined with the
    //   factor `(overload - 1.0)` this means a 100% overrun
    //   (overload = 2.0) at strength = 1.0 requests a 0.35 reduction in
    //   gain, i.e. ≈ 65% of reference — a noticeable but not jarring
    //   dim.
    // - `MIN_REQUESTED_GAIN` (0.45): floor on the requested gain so even
    //   sustained heavy overload cannot crush highlights below ~45% of
    //   reference.
    const MIN_FRAME_BUDGET_MS: f32 = 0.1;
    const OVERLOAD_CLAMP_LO: f32 = 0.5;
    const OVERLOAD_CLAMP_HI: f32 = 3.0;
    const BUDGET_OVERRUN_DAMP: f32 = 0.35;
    const MIN_REQUESTED_GAIN: f32 = 0.45;

    let budget_ms = gaming.frame_time_budget_ms.max(MIN_FRAME_BUDGET_MS);
    let overload = (frame_time_ms / budget_ms).clamp(OVERLOAD_CLAMP_LO, OVERLOAD_CLAMP_HI);
    let requested_gain = if overload <= 1.0 {
        1.0
    } else {
        (1.0 - gaming.anti_pumping_strength * BUDGET_OVERRUN_DAMP * (overload - 1.0))
            .clamp(MIN_REQUESTED_GAIN, 1.0)
    };

    // Rec. 2100 NCL luma weights — matches the rest of QDRV's luminance
    // accounting. The previous simple `(R+G+B)/3` average bled per-channel
    // weighting differences into the temporal controller, particularly for
    // saturated colours where green carries most of the perceived luminance.
    // Sourced from [`qdrv_core::colors::ncl`] so this stays consistent with
    // every other NCL luma site in the workspace.
    use qdrv_core::colors::ncl::{KB, KG, KR};
    let kr = KR as f32;
    let kg = KG as f32;
    let kb = KB as f32;
    let frame_luma = pixels
        .iter()
        .map(|p| kr * p.r + kg * p.g + kb * p.b)
        .sum::<f32>()
        / pixels.len() as f32;

    let allowed_delta = gaming
        .max_gain_delta_per_frame
        .max(0.0)
        .min(v2.temporal.max_global_gain_delta_per_frame.max(0.0));

    // Retrieve the configured integration window size for multi-frame anti-flicker.
    // If not explicitly configured, fall back to the default window size of 12 frames.
    // This value is passed into the temporal state manager to regulate the sliding history buffer.
    let integration_window = v2.temporal.integration_window_frames.unwrap_or(12) as usize;
    let stabilized_gain = state.stabilize_gain(
        requested_gain,
        frame_luma,
        allowed_delta,
        gaming
            .anti_pumping_strength
            .max(v2.temporal.anti_pumping_strength),
        integration_window,
    );

    for p in pixels {
        let mut r = (p.r * stabilized_gain).clamp(0.0, 1.0);
        let mut g = (p.g * stabilized_gain).clamp(0.0, 1.0);
        let mut b = (p.b * stabilized_gain).clamp(0.0, 1.0);
        if matches!(policy.mode, RenderMode::Deterministic) {
            r = ((r * 65_535.0).round() / 65_535.0).clamp(0.0, 1.0);
            g = ((g * 65_535.0).round() / 65_535.0).clamp(0.0, 1.0);
            b = ((b * 65_535.0).round() / 65_535.0).clamp(0.0, 1.0);
        }
        *p = safe_pixel32(r, g, b);
    }
}

// Per-display-class highlight-roll-off bias coefficients applied to the
// adaptation layer's `highlight_rolloff_strength` (which is in [0, 1]).
//
// These are conservative starting heuristics calibrated against the QDRV
// reference adaptation testbench, not normative engineering values. The
// signs encode which display classes need more vs. less highlight
// extension at the reference setting:
//
// - Self-emissive OLED handles highlights well, so we bias *toward* the
//   reference (positive: a small extra brightness multiplier).
// - LCD with global/edge backlight loses local contrast in highlights, so
//   we bias *away* (negative: pull highlights back a touch).
// - MiniLED with local dimming is between OLED and LCD; small positive bias.
// - Projector light output rolls off most aggressively; largest negative
//   bias to compensate.
//
// All four magnitudes are deliberately small (≤ 6% at strength = 1.0) so
// the adaptation never produces a visually disturbing brightness shift
// even at maximum strength. Audit finding S-3 — these were previously
// embedded as unlabelled magic numbers.
const ADAPT_BIAS_OLED: f32 = 0.04;
const ADAPT_BIAS_LCD: f32 = -0.03;
const ADAPT_BIAS_MINILED: f32 = 0.01;
const ADAPT_BIAS_PROJECTOR: f32 = -0.06;

/// Range clamp on `abstract_display_peak_nits / source_mastering_peak_nits`.
/// Below 0.1 the adapter would crush highlights to near-black; above 4.0 it
/// would inflate beyond any sane display capability. The clamp is a sanity
/// guard against pathological adaptation layers carried in metadata.
const ADAPT_PEAK_RATIO_MIN: f32 = 0.1;
const ADAPT_PEAK_RATIO_MAX: f32 = 4.0;

/// Maximum additive shadow lift, scaled by `shadow_lift_strength` (which is
/// in [0, 1]). 0.02 keeps the lift below 1 LSB of a 6-bit signal, which is
/// enough to prevent posterisation in deep shadows on adapted displays
/// while remaining imperceptible on neutral material.
const ADAPT_SHADOW_LIFT_SCALE: f32 = 0.02;

fn apply_display_model_adaptation(
    value: f32,
    layer: &qdrv_meta::open_dynamic_v2::DisplayAdaptationLayer,
    runtime_model: Option<DisplayModelClass>,
) -> f32 {
    let model = runtime_model.unwrap_or(layer.display_model);
    let bias = match model {
        DisplayModelClass::Oled => ADAPT_BIAS_OLED,
        DisplayModelClass::Lcd => ADAPT_BIAS_LCD,
        DisplayModelClass::MiniLed => ADAPT_BIAS_MINILED,
        DisplayModelClass::Projector => ADAPT_BIAS_PROJECTOR,
    };
    let model_bias = 1.0 + layer.highlight_rolloff_strength * bias;
    let peak_ratio = (layer.abstract_display_peak_nits / layer.source_mastering_peak_nits.max(1.0))
        .clamp(ADAPT_PEAK_RATIO_MIN, ADAPT_PEAK_RATIO_MAX);
    let shadow_lift = layer.shadow_lift_strength * ADAPT_SHADOW_LIFT_SCALE;
    ((value * peak_ratio * model_bias) + shadow_lift).clamp(0.0, 1.0)
}

#[inline]
pub(crate) fn sanitise_target_range(target: &TargetDisplay) -> (f32, f32) {
    let mut min_nits = target.min_nits;
    let mut max_nits = target.max_nits;

    if !min_nits.is_finite() || min_nits < 0.0 {
        min_nits = 0.0;
    }
    if !max_nits.is_finite() || max_nits <= 0.0 {
        max_nits = TargetDisplay::default().max_nits;
    }
    if min_nits >= max_nits {
        min_nits = 0.0;
        if min_nits >= max_nits {
            max_nits = TargetDisplay::default().max_nits;
        }
    }
    (min_nits, max_nits)
}

#[inline]
fn sanitise_channel_01(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        // A NaN/Inf reaching this point means an upstream stage produced a
        // non-finite value despite the pipeline-wide clamps. We coerce it to
        // 0.0 to keep the output valid, but loudly panic in debug builds so
        // the root cause is visible during development. Release builds keep
        // the silent fallback so a single bad pixel does not abort rendering.
        debug_assert!(false, "non-finite channel reached sanitise_channel_01: {v}");
        0.0
    }
}

#[inline]
pub(crate) fn safe_pixel32(r: f32, g: f32, b: f32) -> Pixel32 {
    Pixel32::new(r, g, b).unwrap_or_else(|_| {
        Pixel32::new_unchecked(
            sanitise_channel_01(r),
            sanitise_channel_01(g),
            sanitise_channel_01(b),
        )
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use qdrv_meta::{DynamicMeta, ToneMapCurve};

    #[test]
    fn test_tone_map_identity_on_reference_display() {
        // On a 1,000-nit reference display with a linear (identity) curve,
        // pixels whose luminance falls within the display's range must map
        // to approximately the same PQ value. The test inputs (0.1, 0.4, 0.7)
        // all correspond to luminance well below 1,000 nits.
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.tone_map_curve = ToneMapCurve::linear();

        // Use PQ values that correspond to luminance below 1 000 nits.
        let pixels = vec![Pixel32::new_unchecked(0.1, 0.4, 0.7)];
        let target = TargetDisplay::default();
        let mapped = tone_map_frame(&pixels, 1, 1, &dynamic, &target);

        assert!(
            (mapped[0].r - pixels[0].r).abs() < 0.01,
            "R: expected {}, got {}",
            pixels[0].r,
            mapped[0].r
        );
        assert!(
            (mapped[0].g - pixels[0].g).abs() < 0.01,
            "G: expected {}, got {}",
            pixels[0].g,
            mapped[0].g
        );
        assert!(
            (mapped[0].b - pixels[0].b).abs() < 0.01,
            "B: expected {}, got {}",
            pixels[0].b,
            mapped[0].b
        );
    }

    #[test]
    fn test_tone_map_output_clamped() {
        // Tone-mapped output pixels must always contain valid PQ signal values
        // in [0.0, 1.0], regardless of the input or target display capabilities.
        let dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        let pixels = vec![Pixel32::new_unchecked(0.99, 0.99, 0.99)];
        let target = TargetDisplay {
            min_nits: 0.001,
            max_nits: 500.0,
        };
        let mapped = tone_map_frame(&pixels, 1, 1, &dynamic, &target);

        assert!(mapped[0].r >= 0.0 && mapped[0].r <= 1.0);
        assert!(mapped[0].g >= 0.0 && mapped[0].g <= 1.0);
        assert!(mapped[0].b >= 0.0 && mapped[0].b <= 1.0);
    }

    #[test]
    fn test_tone_map_invalid_target_range_is_sanitised() {
        let dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5)];
        let target = TargetDisplay {
            min_nits: 200.0,
            max_nits: 100.0,
        };

        let mapped = tone_map_frame(&pixels, 1, 1, &dynamic, &target);
        assert_eq!(mapped.len(), 1);
        assert!(mapped[0].r.is_finite());
        assert!(mapped[0].g.is_finite());
        assert!(mapped[0].b.is_finite());
    }

    /// Regression test for the M-1 audit finding: confirms that
    /// `local_tone_map_grid` sampling actually varies spatially across the
    /// frame. Before the fix, the tone-mapping path collapsed grid sampling
    /// to a 1-D ramp at `norm_y = 0.5`, so a grid that differed in the
    /// vertical direction would have no observable effect on output.
    #[test]
    fn local_tone_map_grid_varies_with_2d_position() {
        use qdrv_meta::open_dynamic_v2::{
            LocalToneMapCell, LocalToneMapGrid, OpenDynamicMetadataV2,
        };

        // 1x2 grid: top row has a different gain than the bottom row.
        let grid = LocalToneMapGrid {
            cols: 1,
            rows: 2,
            cells: vec![
                LocalToneMapCell {
                    gain: 0.5,
                    offset: 0.0,
                },
                LocalToneMapCell {
                    gain: 1.0,
                    offset: 0.0,
                },
            ],
        };
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.metadata_schema_version = qdrv_meta::compatibility::METADATA_SCHEMA_V2;
        dynamic.tone_map_curve = ToneMapCurve::linear();
        dynamic.open_dynamic_v2 = Some(OpenDynamicMetadataV2 {
            scene_constraints: Vec::new(),
            object_constraints: Vec::new(),
            temporal: Default::default(),
            local_tone_map_grid: Some(grid),
            adaptation_layer: None,
            ambient_policy: None,
            gaming_profile: None,
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        });

        // 2x4 frame: top row (y=0) and bottom row (y=1) at the same column.
        // Note: the per-channel pipeline applies the gain to the curve output
        // (PQ-domain normalised value), so the result is *not* a 0.5x
        // multiplication on the PQ pixel value — but the top vs. bottom rows
        // must produce *different* outputs for the same input pixel.
        let pixel = Pixel32::new_unchecked(0.5, 0.5, 0.5);
        let frame = vec![pixel; 8];
        let target = TargetDisplay::default();
        let out = tone_map_frame(&frame, 2, 4, &dynamic, &target);

        // Index 0 is (col=0, row=0) — sampled from grid row 0 (gain 0.5).
        // Index 6 is (col=0, row=3) — sampled from grid row 1 (gain 1.0).
        let top = out[0].r;
        let bottom = out[6].r;
        assert!(
            (top - bottom).abs() > 1e-3,
            "local_tone_map_grid had no spatial effect: top={top}, bottom={bottom}"
        );
        // Top row uses gain=0.5 → mapped value reduced → final PQ output lower.
        assert!(
            top < bottom,
            "expected top (gain 0.5) below bottom (gain 1.0): top={top}, bottom={bottom}"
        );
    }

    #[test]
    fn temporal_controller_limits_frame_to_frame_gain_delta() {
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.open_dynamic_v2 = Some(qdrv_meta::OpenDynamicMetadataV2 {
            scene_constraints: Vec::new(),
            object_constraints: Vec::new(),
            temporal: qdrv_meta::TemporalConstraint {
                max_global_gain_delta_per_frame: 0.04,
                anti_pumping_strength: 0.8,
                frame_time_budget_ms: Some(8.3),
                integration_window_frames: None,
            },
            local_tone_map_grid: None,
            adaptation_layer: None,
            ambient_policy: None,
            gaming_profile: Some(qdrv_meta::GamingProfile {
                frame_time_budget_ms: 8.3,
                anti_pumping_strength: 0.9,
                max_gain_delta_per_frame: 0.04,
            }),
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        });

        let pixels = vec![Pixel32::new_unchecked(0.75, 0.75, 0.75); 16];
        let target = TargetDisplay::default();
        let mut state = TemporalStateManager::default();

        let policy_slow = RenderPolicy {
            frame_time_ms: Some(25.0),
            ..RenderPolicy::default()
        };
        let out_a = tone_map_frame_with_policy_and_state(
            &pixels,
            4,
            4,
            &dynamic,
            &target,
            &policy_slow,
            &mut state,
        );

        let out_b = tone_map_frame_with_policy_and_state(
            &pixels,
            4,
            4,
            &dynamic,
            &target,
            &policy_slow,
            &mut state,
        );

        let gain_a = out_a[0].r / pixels[0].r;
        let gain_b = out_b[0].r / pixels[0].r;
        assert!((gain_b - gain_a).abs() <= 0.041);
    }

    #[test]
    fn test_multiframe_integration_buffer_suppresses_low_frequency_drift() {
        let mut state_no_window = TemporalStateManager::default();
        let mut state_with_window = TemporalStateManager::default();

        let anti_pumping_strength = 0.8;
        let max_delta = 0.05;
        let frame_luma = 0.5;

        let mut gains_no_window = Vec::new();
        let mut gains_with_window = Vec::new();

        // Simulate a gradual requested gain drift over 20 frames
        for i in 0..20 {
            let requested_gain = 1.0 - 0.015 * i as f32;

            let gain_no = state_no_window.stabilize_gain(
                requested_gain,
                frame_luma,
                max_delta,
                anti_pumping_strength,
                0, // disabled
            );
            gains_no_window.push(gain_no);

            let gain_with = state_with_window.stabilize_gain(
                requested_gain,
                frame_luma,
                max_delta,
                anti_pumping_strength,
                12, // integration window
            );
            gains_with_window.push(gain_with);
        }

        // The final gain with integration window should have drifted significantly less
        // (remained closer to 1.0) than the one without the window.
        let final_gain_no = gains_no_window.last().unwrap();
        let final_gain_with = gains_with_window.last().unwrap();

        assert!(
            final_gain_with > final_gain_no,
            "Expected integration window to suppress drift: with={final_gain_with}, no={final_gain_no}"
        );

        // Check that the difference in drift is clear (e.g. at least 0.02)
        assert!(
            (final_gain_with - final_gain_no) > 0.02,
            "Expected significant suppression: with={final_gain_with}, no={final_gain_no}"
        );
    }
}
