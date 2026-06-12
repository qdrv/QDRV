// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! HDR10+ and HDR10+ Advanced (SMPTE ST 2094-40) sidecar metadata export.
//!
//! Maps QDRV Float32 per-frame dynamic metadata to the integer-valued fields
//! used by the HDR10+ and HDR10+ Advanced specifications. This allows any
//! QDRV file to produce conformant sidecar JSON that can be muxed alongside
//! an HDR10 or HDR10+ Advanced stream.
//!
//! ## HDR10+ (basic) — 10-bit fields
//!
//! | QDRV field (Float32) | HDR10+ field | Conversion |
//! |----------------------|-------------|------------|
//! | `scene_peak_luminance_nits` | `targeted_system_display_maximum_luminance` | Round to nearest integer |
//! | `scene_average_luminance_nits` | `average_maxrgb` | Scale to 10-bit |
//! | `tone_map_curve.anchors` | `bezier_curve_anchors` | Scale to 10-bit |
//!
//! ## HDR10+ Advanced — 16-bit extended fields
//!
//! HDR10+ Advanced (Samsung, CES 2026) extends the base ST 2094-40 structure
//! with 16-bit luminance distribution percentiles, per-channel maxSCL values
//! at 16-bit precision, and an extended colour volume transform.
//!
//! | QDRV field (Float32) | HDR10+ Advanced field | Conversion |
//! |----------------------|----------------------|------------|
//! | `scene_peak_luminance_nits` | `targeted_system_display_maximum_luminance` | Round to nearest integer |
//! | `scene_average_luminance_nits` | `average_maxrgb_16bit` | Scale to 16-bit |
//! | `tone_map_curve.anchors` | `bezier_curve_anchors_16bit` | Scale to 16-bit |
//! | per-channel peak (derived) | `maxscl_16bit[3]` | Scale to 16-bit |
//! | distribution (derived) | `distribution_values_16bit[15]` | 15 percentiles at 16-bit |

use crate::DynamicMeta;
use serde::{Deserialize, Serialize};

const HDR10PLUS_PCT10: [f32; 10] = [0.01, 0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 0.95, 0.98, 0.99];
const HDR10PLUS_PCT15: [f32; 15] = [
    0.01, 0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80, 0.90, 0.95, 0.99,
];
const HDR10PLUS_CEILING_NITS: f32 = 10_000.0;
const HDR10PLUS_PROFILE_EXPORT_SCHEMA_VERSION: u16 = 1;
const HDR10PLUS_COMPATIBILITY_SCHEMA: &str = "qdrv.hdr10plus.compatibility.v1";
const HDR10PLUS_REQUIRED_CERTIFICATION_CAPABILITIES: [&str; 3] = [
    "licensed HDR10+ profile conformance suite",
    "vendor-issued profile certificates and signing chain",
    "proprietary packaging and compliance workflow",
];

/// Upper bound for QDRV's per-frame ambient boost multiplier in the
/// HDR10+ Adaptive mapping. The exporter divides the v2 policy boost by
/// this value before scaling into the 16-bit slot, so values up to ~4×
/// quantise to ~`0xFFFF` and values above saturate. Chosen as a generous
/// realistic ceiling (rooms above ~10 000 lux are direct sunlight and
/// drive a boost ≈ 4× over reference white). Audit finding M-3.
const HDR10PLUS_ADAPTIVE_BOOST_FULL_SCALE: f32 = 4.0;

/// Upper bound for QDRV's per-frame gaming `max_gain_delta_per_frame`
/// quantisation in the HDR10+ Gaming mapping. 0.25 corresponds to a 25%
/// inter-frame tone-map swing — well above the conservative QDRV gaming
/// defaults (0.05) and the broadcast defaults (0.08), so it leaves
/// headroom while keeping the 16-bit slot useful. Audit finding M-3.
const HDR10PLUS_GAMING_GAIN_DELTA_FULL_SCALE: f32 = 0.25;

/// Profile modes exposed by the open HDR10+ exporter.
///
/// Each variant maps to a different per-frame entry shape in the resulting
/// JSON profile export. The mapping from QDRV `DynamicMeta` to the integer
/// HDR10+ fields is documented at the top of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hdr10PlusProfileMode {
    /// SMPTE ST 2094-40 basic (10-bit) profile — the default. Suitable
    /// for the broadest range of HDR10+-capable consumer decoders.
    Basic,
    /// HDR10+ Advanced (16-bit) profile — extends the basic profile with
    /// 16-bit precision luminance distribution, maxSCL, and tone-curve
    /// anchors.
    Advanced,
    /// QDRV open Adaptive-compatible profile — embeds the v2 ambient
    /// policy boost into the Advanced base payload so adaptive decoders
    /// can react to environmental illuminance.
    Adaptive,
    /// QDRV open Gaming-compatible profile — embeds the v2 low-latency
    /// gaming controls (frame-time budget, anti-pumping, gain-delta cap)
    /// into the Advanced base payload.
    Gaming,
}

/// Certification status for an HDR10+ profile export.
///
/// QDRV's open exporter always emits `NotCertified`; the `Certified`
/// variant exists so downstream tooling that consumes a QDRV export and
/// re-emits it via a certified vendor packer can mark its own output
/// without inventing a parallel schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hdr10PlusCertificationStatus {
    /// The export was produced (or re-packaged) through a vendor-licensed
    /// certified HDR10+ tool. **QDRV's open exporter never emits this
    /// variant** — it is provided for downstream consumers that re-emit
    /// QDRV compatibility output through a certified packer.
    Certified,
    /// The export is QDRV's open compatibility-only output. No certified
    /// HDR10+ tooling was involved; downstream certification work, if
    /// required, must run as a separate proprietary step.
    NotCertified,
}

/// Machine-readable compatibility report for one HDR10+ profile mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hdr10PlusCompatibilityReport {
    /// Compatibility report schema identifier.
    pub schema: String,
    /// Export mode this report applies to.
    pub mode: Hdr10PlusProfileMode,
    /// Certification status of generated artefacts.
    pub certification_status: Hdr10PlusCertificationStatus,
    /// True when certified output was produced.
    pub certified_output_generated: bool,
    /// True when a proprietary certification flow is required.
    pub requires_vendor_certification: bool,
    /// Missing capabilities that prevent certified output.
    pub missing_capabilities: Vec<String>,
    /// Human-readable context for operators.
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// HDR10+ (basic) — ST 2094-40, 10-bit fields
// ---------------------------------------------------------------------------

/// A single HDR10+ (ST 2094-40) metadata entry for one frame.
///
/// Contains the fields most commonly used by consumer HDR10+ decoders.
/// All luminance-derived values are quantised to 10-bit (0–1023).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hdr10PlusEntry {
    /// Zero-based index of the frame this sidecar entry describes, aligned with the coded picture order.
    pub frame_index: u64,
    /// Intended peak luminance of the reference display, in nits, as a whole number (not quantised to 10-bit).
    pub targeted_system_display_maximum_luminance: u32,
    /// Scene-average luminance mapped to the HDR10+ Average MaxRGB slot, quantised to 10-bit (0–1023).
    pub average_maxrgb: u16,
    /// Maximum signal level per colour channel at 10-bit precision; see [`to_hdr10plus_entry`] for how QDRV fills this array.
    pub maxscl: [u16; 3],
    /// Histogram-style luminance distribution samples at 10-bit precision, used by tone-mapping adaptors.
    pub distribution_values: Vec<u16>,
    /// Interior Bézier control points (output axis only), quantised to 10-bit; endpoints are omitted by design.
    pub bezier_curve_anchors: Vec<u16>,
    /// Knee point horizontal coordinate at 10-bit precision, taken from the first interior tone-map anchor.
    pub knee_point_x: u16,
    /// Knee point vertical coordinate at 10-bit precision, taken from the first interior tone-map anchor.
    pub knee_point_y: u16,
}

/// Converts a QDRV `DynamicMeta` to a basic HDR10+ (10-bit) sidecar entry.
pub fn to_hdr10plus_entry(meta: &DynamicMeta) -> Hdr10PlusEntry {
    let peak = meta.scene_peak_luminance_nits;
    let avg = meta.scene_average_luminance_nits;

    let peak_10bit = scale_to_bits(peak, 10);
    let avg_10bit = scale_to_bits(avg, 10);

    // Only interior anchors (normalised input strictly between 0 and 1) are serialised into
    // `bezier_curve_anchors`. Endpoints at 0 and 1 are filtered out because the tone-map curve is
    // already implicitly bounded there in HDR10+; including them would duplicate fixed corners and
    // could violate anchor-count limits or decoder expectations for “interior” control points.
    let anchors_10bit: Vec<u16> = meta
        .tone_map_curve
        .anchors
        .iter()
        .filter(|a| a.input > 0.0 && a.input < 1.0)
        .map(|a| (a.output.clamp(0.0, 1.0) * 1023.0).round() as u16)
        .collect();

    let (knee_x, knee_y) = first_interior_anchor(&meta.tone_map_curve, 10);

    Hdr10PlusEntry {
        frame_index: meta.frame_index,
        targeted_system_display_maximum_luminance: peak_nits_to_u32(peak),
        average_maxrgb: avg_10bit,
        // QDRV carries a single scene peak in nits, not separate per-channel maxima. The HDR10+
        // sidecar expects three maxSCL values (R, G, B); we repeat the same quantised peak for each
        // channel so decoders receive a consistent, spec-shaped triple without inventing channel data
        // that the source metadata does not provide.
        maxscl: [peak_10bit, peak_10bit, peak_10bit],
        distribution_values: build_percentiles(peak, 10, 10),
        bezier_curve_anchors: anchors_10bit,
        knee_point_x: knee_x,
        knee_point_y: knee_y,
    }
}

// ---------------------------------------------------------------------------
// HDR10+ Advanced — ST 2094-40 extended, 16-bit fields
// ---------------------------------------------------------------------------

/// 16-bit "neutral" value emitted for HDR10+ Advanced's
/// `colour_saturation_gain` field. The HDR10+ Advanced schema requires
/// this slot, but QDRV's source metadata carries no equivalent — there
/// is no per-frame saturation gain in the QDRV `DynamicMeta` or v2 model
/// to derive a meaningful value from. We emit the spec-mandated mid-scale
/// "no saturation adjustment" sentinel (`0x8000` = ~0.5 of the 0..=1
/// normalised range) so the export is structurally valid for downstream
/// HDR10+ Advanced parsers without claiming any specific saturation
/// intent. Audit finding F-1/M-6.
const HDR10PLUS_ADVANCED_NEUTRAL_SATURATION_GAIN_16BIT: u16 = 32768;

/// A single HDR10+ Advanced metadata entry for one frame.
///
/// Extends the basic [`Hdr10PlusEntry`] with 16-bit precision fields as
/// defined by HDR10+ Advanced (Samsung, CES 2026). The 16-bit fields
/// provide 65,536 levels of quantisation for luminance distribution and
/// tone curve anchors, compared to the 1,024 levels of basic HDR10+.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hdr10PlusAdvancedEntry {
    /// Zero-based frame index; must match the corresponding `DynamicMeta` and basic HDR10+ entry.
    pub frame_index: u64,
    /// Target display peak luminance in nits (integer), shared with the basic HDR10+ representation.
    pub targeted_system_display_maximum_luminance: u32,

    /// Average MaxRGB at 10-bit precision (basic HDR10+ compatibility).
    pub average_maxrgb: u16,
    /// Average MaxRGB at 16-bit precision (HDR10+ Advanced).
    pub average_maxrgb_16bit: u16,

    /// Per-channel maximum scene light level, 10-bit (basic).
    pub maxscl: [u16; 3],
    /// Per-channel maximum scene light level, 16-bit (Advanced).
    pub maxscl_16bit: [u16; 3],

    /// 10 luminance distribution percentiles at 10-bit (basic).
    pub distribution_values: Vec<u16>,
    /// 15 luminance distribution percentiles at 16-bit (Advanced).
    pub distribution_values_16bit: Vec<u16>,

    /// Bézier curve anchors at 10-bit (basic).
    pub bezier_curve_anchors: Vec<u16>,
    /// Bézier curve anchors at 16-bit (Advanced).
    pub bezier_curve_anchors_16bit: Vec<u16>,

    /// Knee point X at 10-bit precision, mirroring the basic HDR10+ knee fields for backwards compatibility.
    pub knee_point_x: u16,
    /// Knee point Y at 10-bit precision, mirroring the basic HDR10+ knee fields for backwards compatibility.
    pub knee_point_y: u16,
    /// Knee point X at 16-bit precision (Advanced).
    pub knee_point_x_16bit: u16,
    /// Knee point Y at 16-bit precision (Advanced).
    pub knee_point_y_16bit: u16,

    /// Colour saturation gain for the highlight region (Advanced).
    /// Normalised to [0.0, 1.0] and quantised to 16-bit.
    pub colour_saturation_gain: u16,
}

/// Open HDR10+ Adaptive profile entry.
///
/// QDRV maps ambient adaptation policy signals from Open Dynamic Metadata v2 to
/// this profile while retaining the full HDR10+ Advanced base payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hdr10PlusAdaptiveEntry {
    /// HDR10+ Advanced base payload retained for decoder compatibility.
    pub base: Hdr10PlusAdvancedEntry,
    /// Representative ambient illuminance used for this profile mapping.
    pub ambient_lux: f32,
    /// Ambient boost multiplier sampled from v2 policy.
    pub ambient_boost_multiplier: f32,
    /// Ambient boost multiplier quantised to 16-bit.
    pub ambient_boost_multiplier_16bit: u16,
    /// Maximum ambient-policy delta per second quantised to 16-bit.
    pub adaptation_max_delta_per_second_16bit: u16,
}

/// Open HDR10+ Gaming-equivalent profile entry.
///
/// QDRV maps low-latency temporal controls from Open Dynamic Metadata v2 while
/// retaining the HDR10+ Advanced base payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hdr10PlusGamingEntry {
    /// HDR10+ Advanced base payload retained for decoder compatibility.
    pub base: Hdr10PlusAdvancedEntry,
    /// Frame-time budget associated with this frame.
    pub frame_time_budget_ms: f32,
    /// Anti-pumping strength quantised to 16-bit.
    pub anti_pumping_strength_16bit: u16,
    /// Maximum gain delta per frame quantised to 16-bit.
    pub max_gain_delta_per_frame_16bit: u16,
}

/// Tagged per-frame entry variant for profile-aware HDR10+ exports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "profile", content = "entry", rename_all = "snake_case")]
pub enum Hdr10PlusProfiledEntry {
    Basic(Hdr10PlusEntry),
    Advanced(Hdr10PlusAdvancedEntry),
    Adaptive(Hdr10PlusAdaptiveEntry),
    Gaming(Hdr10PlusGamingEntry),
}

/// Top-level profile export payload written by `qdrv hdr10plus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hdr10PlusProfileExport {
    /// Profile export schema version.
    pub schema_version: u16,
    /// Requested exporter mode.
    pub mode: Hdr10PlusProfileMode,
    /// Explicit compatibility report with certification status.
    pub compatibility: Hdr10PlusCompatibilityReport,
    /// Per-frame metadata entries encoded for the requested mode.
    pub entries: Vec<Hdr10PlusProfiledEntry>,
}

/// Converts a QDRV `DynamicMeta` to an HDR10+ Advanced (16-bit) sidecar entry.
///
/// The output includes both the 10-bit basic HDR10+ fields (for backwards
/// compatibility with basic HDR10+ decoders) and the 16-bit extended fields
/// used by HDR10+ Advanced decoders.
pub fn to_hdr10plus_advanced_entry(meta: &DynamicMeta) -> Hdr10PlusAdvancedEntry {
    let peak = meta.scene_peak_luminance_nits;
    let avg = meta.scene_average_luminance_nits;

    let peak_10bit = scale_to_bits(peak, 10);
    let avg_10bit = scale_to_bits(avg, 10);
    let peak_16bit = scale_to_bits(peak, 16);
    let avg_16bit = scale_to_bits(avg, 16);

    let anchors_10bit: Vec<u16> = meta
        .tone_map_curve
        .anchors
        .iter()
        .filter(|a| a.input > 0.0 && a.input < 1.0)
        .map(|a| (a.output.clamp(0.0, 1.0) * 1023.0).round() as u16)
        .collect();

    let anchors_16bit: Vec<u16> = meta
        .tone_map_curve
        .anchors
        .iter()
        .filter(|a| a.input > 0.0 && a.input < 1.0)
        .map(|a| (a.output.clamp(0.0, 1.0) * 65535.0).round() as u16)
        .collect();

    let (knee_x_10, knee_y_10) = first_interior_anchor(&meta.tone_map_curve, 10);
    let (knee_x_16, knee_y_16) = first_interior_anchor(&meta.tone_map_curve, 16);

    Hdr10PlusAdvancedEntry {
        frame_index: meta.frame_index,
        targeted_system_display_maximum_luminance: peak_nits_to_u32(peak),
        average_maxrgb: avg_10bit,
        average_maxrgb_16bit: avg_16bit,
        maxscl: [peak_10bit, peak_10bit, peak_10bit],
        maxscl_16bit: [peak_16bit, peak_16bit, peak_16bit],
        distribution_values: build_percentiles(peak, 10, 10),
        distribution_values_16bit: build_percentiles(peak, 15, 16),
        bezier_curve_anchors: anchors_10bit,
        bezier_curve_anchors_16bit: anchors_16bit,
        knee_point_x: knee_x_10,
        knee_point_y: knee_y_10,
        knee_point_x_16bit: knee_x_16,
        knee_point_y_16bit: knee_y_16,
        colour_saturation_gain: HDR10PLUS_ADVANCED_NEUTRAL_SATURATION_GAIN_16BIT,
    }
}

/// Converts a QDRV `DynamicMeta` to an open HDR10+ Adaptive profile entry.
pub fn to_hdr10plus_adaptive_entry(meta: &DynamicMeta) -> Hdr10PlusAdaptiveEntry {
    let base = to_hdr10plus_advanced_entry(meta);
    let (ambient_lux, ambient_boost_multiplier, max_delta_per_second) = meta
        .open_dynamic_v2
        .as_ref()
        .and_then(|v2| v2.ambient_policy.as_ref())
        .map(|policy| {
            let ambient_lux = representative_ambient_lux(policy);
            (
                ambient_lux,
                policy.boost_for_lux(ambient_lux),
                policy.max_delta_per_second,
            )
        })
        .unwrap_or((0.0, 1.0, 0.0));

    Hdr10PlusAdaptiveEntry {
        base,
        ambient_lux,
        ambient_boost_multiplier,
        ambient_boost_multiplier_16bit: scale_unit_interval_to_u16(
            ambient_boost_multiplier / HDR10PLUS_ADAPTIVE_BOOST_FULL_SCALE,
        ),
        adaptation_max_delta_per_second_16bit: scale_unit_interval_to_u16(
            (max_delta_per_second / HDR10PLUS_ADAPTIVE_BOOST_FULL_SCALE).clamp(0.0, 1.0),
        ),
    }
}

/// Converts a QDRV `DynamicMeta` to an open HDR10+ Gaming-equivalent entry.
pub fn to_hdr10plus_gaming_entry(meta: &DynamicMeta) -> Hdr10PlusGamingEntry {
    let base = to_hdr10plus_advanced_entry(meta);
    let (frame_time_budget_ms, anti_pumping_strength, max_gain_delta_per_frame) =
        if let Some(v2) = meta.open_dynamic_v2.as_ref() {
            if let Some(gaming) = v2.gaming_profile.as_ref() {
                (
                    gaming.frame_time_budget_ms,
                    gaming.anti_pumping_strength,
                    gaming.max_gain_delta_per_frame,
                )
            } else {
                (
                    v2.temporal.frame_time_budget_ms.unwrap_or(8.3),
                    v2.temporal.anti_pumping_strength,
                    v2.temporal.max_global_gain_delta_per_frame,
                )
            }
        } else {
            (8.3, 0.8, 0.05)
        };

    Hdr10PlusGamingEntry {
        base,
        frame_time_budget_ms,
        anti_pumping_strength_16bit: scale_unit_interval_to_u16(anti_pumping_strength),
        max_gain_delta_per_frame_16bit: scale_unit_interval_to_u16(
            (max_gain_delta_per_frame / HDR10PLUS_GAMING_GAIN_DELTA_FULL_SCALE).clamp(0.0, 1.0),
        ),
    }
}

/// Builds a machine-readable profile export payload for a sequence of frames.
pub fn build_profile_export(
    metas: &[DynamicMeta],
    mode: Hdr10PlusProfileMode,
) -> Hdr10PlusProfileExport {
    let entries = metas
        .iter()
        .map(|meta| match mode {
            Hdr10PlusProfileMode::Basic => Hdr10PlusProfiledEntry::Basic(to_hdr10plus_entry(meta)),
            Hdr10PlusProfileMode::Advanced => {
                Hdr10PlusProfiledEntry::Advanced(to_hdr10plus_advanced_entry(meta))
            }
            Hdr10PlusProfileMode::Adaptive => {
                Hdr10PlusProfiledEntry::Adaptive(to_hdr10plus_adaptive_entry(meta))
            }
            Hdr10PlusProfileMode::Gaming => {
                Hdr10PlusProfiledEntry::Gaming(to_hdr10plus_gaming_entry(meta))
            }
        })
        .collect();

    Hdr10PlusProfileExport {
        schema_version: HDR10PLUS_PROFILE_EXPORT_SCHEMA_VERSION,
        mode,
        compatibility: compatibility_report(mode),
        entries,
    }
}

/// Returns a strict compatibility report with explicit non-certification markers.
pub fn compatibility_report(mode: Hdr10PlusProfileMode) -> Hdr10PlusCompatibilityReport {
    let profile_note = match mode {
        Hdr10PlusProfileMode::Basic => {
            "Exports ST 2094-40 basic fields from QDRV metadata in open mode"
        }
        Hdr10PlusProfileMode::Advanced => {
            "Exports HDR10+ Advanced-equivalent 16-bit fields from QDRV metadata in open mode"
        }
        Hdr10PlusProfileMode::Adaptive => {
            "Maps Open Dynamic Metadata v2 ambient policy into an HDR10+ Adaptive profile extension"
        }
        Hdr10PlusProfileMode::Gaming => {
            "Maps Open Dynamic Metadata v2 temporal/gaming controls into a gaming-equivalent profile extension"
        }
    };

    Hdr10PlusCompatibilityReport {
        schema: HDR10PLUS_COMPATIBILITY_SCHEMA.to_string(),
        mode,
        certification_status: Hdr10PlusCertificationStatus::NotCertified,
        certified_output_generated: false,
        requires_vendor_certification: true,
        missing_capabilities: HDR10PLUS_REQUIRED_CERTIFICATION_CAPABILITIES
            .iter()
            .map(|v| v.to_string())
            .collect(),
        notes: vec![
            "Open exporter output is intended for compatibility workflows, not certification delivery"
                .to_string(),
            profile_note.to_string(),
        ],
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Scales a nit value to an N-bit integer in [0, 2^N - 1].
fn scale_to_bits(nits: f32, bits: u8) -> u16 {
    scale_to_bits_saturated(nits, bits).0
}

/// Variant of [`scale_to_bits`] that reports whether input was clamped.
///
/// `bits` is silently clamped to `1..=16` so callers cannot trigger
/// `1u32 << bits` overflow for `bits >= 32` (which would panic in debug
/// builds and wrap in release).
fn scale_to_bits_saturated(nits: f32, bits: u8) -> (u16, bool) {
    debug_assert!(
        (1..=16).contains(&bits),
        "scale_to_bits_saturated bits must be in 1..=16, got {bits}"
    );
    let bits = bits.clamp(1, 16);
    let max_val = (((1u32 << bits) - 1) as f32).max(1.0);
    let normalised = nits / HDR10PLUS_CEILING_NITS;
    let saturated = !normalised.is_finite() || !(0.0..=1.0).contains(&normalised);
    let scaled = normalised.clamp(0.0, 1.0);
    (((scaled * max_val).round() as u16), saturated)
}

/// Clamps a finite peak luminance in nits into a non-negative `u32` rather
/// than relying on saturating `as u32` cast behaviour for `INFINITY` or NaN.
fn peak_nits_to_u32(peak: f32) -> u32 {
    if !peak.is_finite() {
        return 0;
    }
    peak.max(0.0).min(u32::MAX as f32).round() as u32
}

/// Builds evenly-spaced luminance distribution percentiles.
fn build_percentiles(peak_nits: f32, count: usize, bits: u8) -> Vec<u16> {
    let positions: &[f32] = match count {
        10 => &HDR10PLUS_PCT10,
        15 => &HDR10PLUS_PCT15,
        _ => {
            return (0..count)
                .map(|i| {
                    let denom = count.saturating_sub(1).max(1);
                    let frac = i as f32 / denom as f32;
                    scale_to_bits(frac * peak_nits, bits)
                })
                .collect();
        }
    };

    positions
        .iter()
        .map(|&frac| scale_to_bits(frac * peak_nits, bits))
        .collect()
}

/// Returns the first interior anchor point scaled to N-bit precision.
fn first_interior_anchor(curve: &crate::ToneMapCurve, bits: u8) -> (u16, u16) {
    let max_val = ((1u32 << bits) - 1) as f32;
    curve
        .anchors
        .iter()
        .find(|a| a.input > 0.0 && a.input < 1.0)
        .map(|a| {
            let x = (a.input.clamp(0.0, 1.0) * max_val).round() as u16;
            let y = (a.output.clamp(0.0, 1.0) * max_val).round() as u16;
            (x, y)
        })
        .unwrap_or((0, 0))
}

/// Picks a single representative ambient illuminance value for the HDR10+
/// Adaptive export from a v2 ambient policy curve.
///
/// HDR10+ Adaptive carries one ambient value per frame; QDRV's v2 ambient
/// policy is a curve over many lux breakpoints. We pick the **median
/// breakpoint** (index `len / 2`, integer division) as a representative
/// sample because:
///
/// - the breakpoint vector is strictly monotonic (validated upstream), so
///   the median is also a numerical median;
/// - it is independent of the boost-multiplier ordering, so adversarial
///   policies cannot make us pick a pathologically high or low value;
/// - it gives consumer decoders that don't carry a curve a sensible
///   midpoint they can compensate around.
///
/// Audit finding M-4.
fn representative_ambient_lux(policy: &crate::open_dynamic_v2::AmbientAdaptivePolicy) -> f32 {
    match policy.lux_breakpoints.len() {
        0 => 0.0,
        len => policy.lux_breakpoints[len / 2].max(0.0),
    }
}

fn scale_unit_interval_to_u16(value: f32) -> u16 {
    (value.clamp(0.0, 1.0) * 65535.0).round() as u16
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DynamicMeta,
        open_dynamic_v2::{AmbientAdaptivePolicy, GamingProfile, OpenDynamicMetadataV2},
    };

    /// Verifies that a minimal `DynamicMeta` round-trips into a basic HDR10+ entry with sane ranges and counts.
    #[test]
    fn test_hdr10plus_basic_export() {
        let meta = DynamicMeta::new(0, 1000.0, 200.0);
        let entry = to_hdr10plus_entry(&meta);
        assert_eq!(entry.frame_index, 0);
        assert_eq!(entry.targeted_system_display_maximum_luminance, 1000);
        assert!(entry.average_maxrgb <= 1023);
        assert!(entry.bezier_curve_anchors.len() <= 32);
        assert_eq!(entry.distribution_values.len(), 10);
    }

    /// Ensures 10-bit anchors and distribution samples from a high-luminance scene stay within the 0–1023 range.
    #[test]
    fn test_hdr10plus_basic_anchors_in_range() {
        let meta = DynamicMeta::new(5, 4000.0, 500.0);
        let entry = to_hdr10plus_entry(&meta);
        for &v in &entry.bezier_curve_anchors {
            assert!(v <= 1023, "anchor value {v} exceeds 10-bit range");
        }
        for &v in &entry.distribution_values {
            assert!(v <= 1023, "percentile value {v} exceeds 10-bit range");
        }
    }

    /// Checks that an advanced HDR10+ entry exposes both 10-bit and 16-bit fields with expected vector lengths.
    #[test]
    fn test_hdr10plus_advanced_export() {
        let meta = DynamicMeta::new(0, 4000.0, 500.0);
        let entry = to_hdr10plus_advanced_entry(&meta);

        assert_eq!(entry.frame_index, 0);
        assert_eq!(entry.targeted_system_display_maximum_luminance, 4000);

        assert!(entry.average_maxrgb <= 1023);
        // `average_maxrgb_16bit` is a `u16`, so it is always ≤ 65 535; the meaningful check is
        // that scaling to 16-bit does not undershoot the coarser 10-bit quantisation.
        assert!(
            entry.average_maxrgb_16bit >= entry.average_maxrgb,
            "16-bit average should be >= 10-bit average"
        );

        assert_eq!(entry.distribution_values.len(), 10);
        assert_eq!(entry.distribution_values_16bit.len(), 15);
    }

    /// Confirms all 16-bit quantised arrays and `maxscl_16bit` remain within the 0–65535 envelope.
    #[test]
    fn test_hdr10plus_advanced_16bit_precision() {
        let meta = DynamicMeta::new(0, 1000.0, 200.0);
        let entry = to_hdr10plus_advanced_entry(&meta);

        // All Advanced luminance slots are `u16`; non-emptiness and length invariants are the
        // useful checks here (range is enforced by the type system).
        assert!(!entry.bezier_curve_anchors_16bit.is_empty());
        assert_eq!(entry.distribution_values_16bit.len(), 15);
        assert_eq!(entry.maxscl_16bit.len(), 3);
    }

    /// Asserts that shared fields between basic and advanced exports match when built from the same `DynamicMeta`.
    #[test]
    fn test_hdr10plus_advanced_backwards_compatible() {
        let meta = DynamicMeta::new(0, 2000.0, 300.0);
        let basic = to_hdr10plus_entry(&meta);
        let advanced = to_hdr10plus_advanced_entry(&meta);

        assert_eq!(basic.frame_index, advanced.frame_index);
        assert_eq!(
            basic.targeted_system_display_maximum_luminance,
            advanced.targeted_system_display_maximum_luminance
        );
        assert_eq!(basic.average_maxrgb, advanced.average_maxrgb);
        assert_eq!(basic.maxscl, advanced.maxscl);
        assert_eq!(basic.knee_point_x, advanced.knee_point_x);
        assert_eq!(basic.knee_point_y, advanced.knee_point_y);
        assert_eq!(basic.bezier_curve_anchors, advanced.bezier_curve_anchors);
    }

    /// Exercises `scale_to_bits` at 0 nits, full-scale 10 000 nits, and mid-scale 5 000 nits for 10- and 16-bit paths.
    #[test]
    fn test_scale_to_bits_boundaries() {
        assert_eq!(scale_to_bits(0.0, 10), 0);
        assert_eq!(scale_to_bits(10_000.0, 10), 1023);
        assert_eq!(scale_to_bits(0.0, 16), 0);
        assert_eq!(scale_to_bits(10_000.0, 16), 65535);
        assert_eq!(scale_to_bits(5_000.0, 16), 32768);
    }

    #[test]
    fn test_build_percentiles_uses_spec_positions_for_10_slot_output() {
        let values = build_percentiles(10_000.0, 10, 10);
        assert_eq!(values.len(), 10);
        assert_eq!(values[0], 10); // 1%
        assert_eq!(values[1], 51); // 5%
        assert_eq!(values[2], 102); // 10%
        assert_eq!(values[9], 1013); // 99%
    }

    #[test]
    fn test_scale_to_bits_saturated_reports_clamping() {
        let (_, sat_hi) = scale_to_bits_saturated(20_000.0, 10);
        let (_, sat_lo) = scale_to_bits_saturated(-1.0, 10);
        let (_, sat_ok) = scale_to_bits_saturated(1_000.0, 10);
        assert!(sat_hi);
        assert!(sat_lo);
        assert!(!sat_ok);
    }

    #[test]
    fn test_build_percentiles_zero_count_returns_empty() {
        let values = build_percentiles(10_000.0, 0, 10);
        assert!(values.is_empty());
    }

    #[test]
    fn test_hdr10plus_adaptive_export_contains_mode_fields() {
        let mut meta = DynamicMeta::new(3, 1200.0, 220.0);
        meta.open_dynamic_v2 = Some(OpenDynamicMetadataV2 {
            scene_constraints: Vec::new(),
            object_constraints: Vec::new(),
            temporal: Default::default(),
            local_tone_map_grid: None,
            adaptation_layer: None,
            ambient_policy: Some(AmbientAdaptivePolicy {
                lux_breakpoints: vec![0.0, 120.0, 500.0],
                boost_multipliers: vec![1.0, 1.15, 1.3],
                max_delta_per_second: 0.6,
            }),
            gaming_profile: None,
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        });
        let entry = to_hdr10plus_adaptive_entry(&meta);
        assert_eq!(entry.base.frame_index, 3);
        assert!(entry.ambient_lux >= 0.0);
        assert!(entry.ambient_boost_multiplier >= 1.0);
        assert!(entry.ambient_boost_multiplier_16bit > 0);
    }

    #[test]
    fn test_hdr10plus_gaming_export_contains_mode_fields() {
        let mut meta = DynamicMeta::new(4, 1500.0, 280.0);
        meta.open_dynamic_v2 = Some(OpenDynamicMetadataV2 {
            scene_constraints: Vec::new(),
            object_constraints: Vec::new(),
            temporal: Default::default(),
            local_tone_map_grid: None,
            adaptation_layer: None,
            ambient_policy: None,
            gaming_profile: Some(GamingProfile {
                frame_time_budget_ms: 6.9,
                anti_pumping_strength: 0.77,
                max_gain_delta_per_frame: 0.04,
            }),
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        });
        let entry = to_hdr10plus_gaming_entry(&meta);
        assert_eq!(entry.base.frame_index, 4);
        assert!(entry.frame_time_budget_ms > 0.0);
        assert!(entry.anti_pumping_strength_16bit > 0);
    }

    #[test]
    fn test_profile_export_includes_mode_and_not_certified_marker() {
        let metas = vec![DynamicMeta::new(0, 1000.0, 200.0)];
        for mode in [
            Hdr10PlusProfileMode::Basic,
            Hdr10PlusProfileMode::Advanced,
            Hdr10PlusProfileMode::Adaptive,
            Hdr10PlusProfileMode::Gaming,
        ] {
            let export = build_profile_export(&metas, mode);
            assert_eq!(export.mode, mode);
            assert_eq!(
                export.schema_version,
                HDR10PLUS_PROFILE_EXPORT_SCHEMA_VERSION
            );
            assert_eq!(
                export.compatibility.certification_status,
                Hdr10PlusCertificationStatus::NotCertified
            );
            assert!(!export.compatibility.certified_output_generated);
            assert_eq!(export.entries.len(), 1);
        }
    }
}
