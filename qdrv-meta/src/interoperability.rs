// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Interoperability-first exporter models and explicit loss reporting.

use serde::{Deserialize, Serialize};

use crate::DynamicMeta;

/// Proprietary capabilities required for certified Dolby Vision packaging.
pub const DV_REQUIRED_PROPRIETARY_CAPABILITIES: [&str; 3] = [
    "certified Dolby Vision bitstream packer",
    "vendor signing keys and profile certificates",
    "licensed proprietary RPU block serializer",
];

/// "Unsupported feature" string emitted by [`dolby_vision_compatible_sidecar`]
/// for the missing certified DV bitstream capability. Exported as a constant
/// so the `qdrv-tool` interop exporter can remove it from the report on a
/// successful adapter run without depending on a hand-copied string literal
/// (audit finding GG-5).
pub const DV_LOSS_UNSUPPORTED_CERTIFIED_BITSTREAM: &str =
    "generation of certified Dolby Vision bitstream";

/// "Unsupported feature" string emitted by [`dolby_vision_compatible_sidecar`]
/// for the missing vendor-key carriage capability. See
/// [`DV_LOSS_UNSUPPORTED_CERTIFIED_BITSTREAM`] for the rationale.
pub const DV_LOSS_UNSUPPORTED_VENDOR_KEYS: &str = "private metadata carriage and vendor keys";

/// Target interoperability export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteropTarget {
    /// Static HDR10-compatible output.
    Hdr10,
    /// Dynamic HDR10+ JSON/sidecar output.
    Hdr10Plus,
    /// Dolby Vision compatible sidecar representation.
    DolbyVisionCompatible,
}

/// Field-level fidelity and feature loss report for interoperability export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteropLossReport {
    /// Target format for which this loss assessment was generated.
    pub target: InteropTarget,
    /// Metadata fields that cannot be represented and are dropped entirely.
    pub dropped_fields: Vec<String>,
    /// Metadata fields that are retained only through approximation or quantised mapping.
    pub approximated_fields: Vec<String>,
    /// Features unsupported by this open exporter implementation.
    pub unsupported_features: Vec<String>,
}

/// Operational mode for Dolby Vision-compatible sidecar exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DolbyVisionCompatibilityMode {
    /// Open sidecar output only; no proprietary packer output was generated.
    OpenSidecarOnly,
    /// Proprietary packer successfully generated certified DV-compatible output.
    ProprietaryAdapterPackaged,
}

/// Machine-readable compatibility metadata for DV-compatible exports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DolbyVisionCompatibilityMetadata {
    /// Operational mode used for this sidecar.
    pub mode: DolbyVisionCompatibilityMode,
    /// Sidecar schema identifier for downstream tooling.
    pub sidecar_schema: String,
    /// True when proprietary packaging is needed for certified output.
    pub requires_proprietary_packer: bool,
    /// True when a proprietary packer was available during export.
    pub proprietary_packer_available: bool,
    /// True when certified DV-compatible output was generated.
    pub certified_output_generated: bool,
    /// Missing proprietary capabilities that blocked certified output.
    pub missing_capabilities: Vec<String>,
}

impl DolbyVisionCompatibilityMetadata {
    /// Compatibility metadata for open sidecar-only operation.
    pub fn open_sidecar_only() -> Self {
        Self {
            mode: DolbyVisionCompatibilityMode::OpenSidecarOnly,
            sidecar_schema: "qdrv.dv-compatible.sidecar.v1".to_string(),
            requires_proprietary_packer: true,
            proprietary_packer_available: false,
            certified_output_generated: false,
            missing_capabilities: DV_REQUIRED_PROPRIETARY_CAPABILITIES
                .iter()
                .map(|v| v.to_string())
                .collect(),
        }
    }

    /// Compatibility metadata for successful proprietary adapter packaging.
    pub fn proprietary_adapter_packaged() -> Self {
        Self {
            mode: DolbyVisionCompatibilityMode::ProprietaryAdapterPackaged,
            sidecar_schema: "qdrv.dv-compatible.sidecar.v1".to_string(),
            requires_proprietary_packer: true,
            proprietary_packer_available: true,
            certified_output_generated: true,
            missing_capabilities: Vec::new(),
        }
    }
}

/// Open Dolby Vision-compatible sidecar payload (non-proprietary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DolbyVisionCompatibleSidecar {
    /// Frame index.
    pub frame_index: u64,
    /// Scene peak luminance as a **linear-light fraction of 10 000 nits**
    /// — i.e. `scene_peak_luminance_nits / 10_000.0`, clamped to
    /// `[0.0, 1.0]`. This is a normalised linear ratio, **not** the PQ-
    /// encoded signal value at that luminance. The field used to be named
    /// `target_max_pq`, but the value never went through the PQ OETF; the
    /// rename is the O-1 audit fix that brings the name and the value into
    /// agreement. Consumers that want a PQ code can compute it themselves
    /// via `qdrv_core::pq::pq_oetf_f32` on this value.
    pub target_max_linear_fraction: f32,
    /// Approximate dynamic curve anchors.
    pub curve_anchors: Vec<(f32, f32)>,
    /// True only when proprietary DV bitstream generation is available.
    pub proprietary_bitstream_generated: bool,
    /// Capability/interop notes.
    pub notes: Vec<String>,
    /// Explicit machine-readable compatibility metadata.
    pub compatibility: DolbyVisionCompatibilityMetadata,
}

/// Produces an explicit HDR10 loss report from QDRV dynamic metadata.
pub fn hdr10_loss_report(meta: &DynamicMeta) -> InteropLossReport {
    let mut approximated = vec![
        "tone_map_curve -> static transfer with quantized 10-bit encoding".to_string(),
        "scene_peak_luminance_nits -> maxcll integer rounding".to_string(),
        "scene_average_luminance_nits -> maxfall integer rounding".to_string(),
    ];
    let mut dropped = vec!["target_display_hint.min_luminance_nits".to_string()];
    let mut unsupported = Vec::new();
    if meta.open_dynamic_v2.is_some() {
        dropped.push("open_dynamic_v2.local_tone_map_grid".to_string());
        dropped.push("open_dynamic_v2.object_constraints".to_string());
        dropped.push("open_dynamic_v2.temporal".to_string());
        dropped.push("open_dynamic_v2.ambient_policy".to_string());
        approximated.push(
            "open_dynamic_v2.adaptation_layer -> static mastering display assumption".to_string(),
        );
        unsupported.push("open_dynamic_v2 inverse reconstruction hints".to_string());
    }

    InteropLossReport {
        target: InteropTarget::Hdr10,
        dropped_fields: dropped,
        approximated_fields: approximated,
        unsupported_features: unsupported,
    }
}

/// Produces a DV-compatible open sidecar and explicit loss report.
pub fn dolby_vision_compatible_sidecar(
    meta: &DynamicMeta,
) -> (DolbyVisionCompatibleSidecar, InteropLossReport) {
    let target_max_linear_fraction = (meta.scene_peak_luminance_nits / 10_000.0).clamp(0.0, 1.0);
    let anchors = meta
        .tone_map_curve
        .anchors
        .iter()
        .map(|a| (a.input, a.output))
        .collect::<Vec<_>>();

    let sidecar = DolbyVisionCompatibleSidecar {
        frame_index: meta.frame_index,
        target_max_linear_fraction,
        curve_anchors: anchors,
        proprietary_bitstream_generated: false,
        notes: vec![
            "Open DV-compatible sidecar only; proprietary RPU bitstream generation unavailable"
                .to_string(),
            "Use this payload as an intermediate for downstream proprietary packaging".to_string(),
        ],
        compatibility: DolbyVisionCompatibilityMetadata::open_sidecar_only(),
    };

    let report = InteropLossReport {
        target: InteropTarget::DolbyVisionCompatible,
        dropped_fields: vec![
            "proprietary_dolby_rpu_extension_blocks".to_string(),
            "vendor-specific profile negotiation".to_string(),
        ],
        approximated_fields: vec![
            "tone_map_curve anchors mapped into open sidecar schema".to_string(),
            "scene luminance mapped to target_max_linear_fraction".to_string(),
        ],
        unsupported_features: vec![
            DV_LOSS_UNSUPPORTED_CERTIFIED_BITSTREAM.to_string(),
            DV_LOSS_UNSUPPORTED_VENDOR_KEYS.to_string(),
        ],
    };

    (sidecar, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dv_sidecar_includes_open_compatibility_metadata() {
        let meta = DynamicMeta::new(7, 1200.0, 250.0);
        let (sidecar, report) = dolby_vision_compatible_sidecar(&meta);
        assert_eq!(sidecar.frame_index, 7);
        assert!(matches!(
            sidecar.compatibility.mode,
            DolbyVisionCompatibilityMode::OpenSidecarOnly
        ));
        assert!(sidecar.compatibility.requires_proprietary_packer);
        assert!(!sidecar.compatibility.certified_output_generated);
        assert!(!sidecar.compatibility.missing_capabilities.is_empty());
        assert_eq!(report.target, InteropTarget::DolbyVisionCompatible);
    }

    #[test]
    fn proprietary_packaged_metadata_clears_missing_capabilities() {
        let packaged = DolbyVisionCompatibilityMetadata::proprietary_adapter_packaged();
        assert!(matches!(
            packaged.mode,
            DolbyVisionCompatibilityMode::ProprietaryAdapterPackaged
        ));
        assert!(packaged.proprietary_packer_available);
        assert!(packaged.certified_output_generated);
        assert!(packaged.missing_capabilities.is_empty());
    }
}
