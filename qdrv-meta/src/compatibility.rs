// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Metadata version compatibility and strict schema policy rules.

use serde::{Deserialize, Serialize};

use crate::{DynamicMeta, StaticMeta, Tier};

/// Known metadata schema versions.
pub const METADATA_SCHEMA_V1: u16 = 1;
/// Open Dynamic Metadata v2 schema.
pub const METADATA_SCHEMA_V2: u16 = 2;
/// Current writer target.
pub const CURRENT_METADATA_SCHEMA: u16 = METADATA_SCHEMA_V2;

/// Canonical QDRV **format** version string written to every static
/// metadata block. Single source of truth so the JSON `qdrv_version`
/// field, the legacy v1 binary decode, and the project's documented
/// release version cannot drift apart — historically (U-9) the default
/// constructors emitted `"0.2.0"` while the binary v1 decode hardcoded
/// `"0.1.0"`, producing files whose declared version depended on which
/// encoding path produced them.
///
/// Kept in sync with the workspace `version` in the root `Cargo.toml`
/// and the version banners in `README.md`, `docs/QDRV_SPEC.md`, and
/// `docs/QDRV_TECHNICAL_REFERENCE.md`.
pub const QDRV_FORMAT_VERSION: &str = "0.1.0";

/// Compatibility behaviour policy for metadata parsers and writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityMode {
    /// Accept only the current schema version for strict forward deployments.
    StrictCurrent,
    /// Accept both the current schema and the legacy v1 payload shape.
    BackwardCompatible,
}

/// Runtime policy that controls schema acceptance and writer output versioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityPolicy {
    /// Reader/writer compatibility mode.
    pub mode: CompatibilityMode,
    /// Schema version to emit when serialising.
    pub write_schema_version: u16,
}

impl Default for CompatibilityPolicy {
    fn default() -> Self {
        Self {
            mode: CompatibilityMode::BackwardCompatible,
            write_schema_version: CURRENT_METADATA_SCHEMA,
        }
    }
}

/// Validates static/dynamic metadata schema compatibility and cross-field rules.
pub fn validate_compatibility(
    static_meta: &StaticMeta,
    dynamic_meta: &DynamicMeta,
    policy: CompatibilityPolicy,
) -> Result<(), &'static str> {
    if static_meta.metadata_schema_version != dynamic_meta.metadata_schema_version {
        return Err("static and dynamic metadata schema versions must match");
    }

    if static_meta.metadata_schema_version == METADATA_SCHEMA_V2
        && dynamic_meta.open_dynamic_v2.is_none()
    {
        return Err("schema v2 requires open_dynamic_v2 payload");
    }
    if static_meta.metadata_schema_version == METADATA_SCHEMA_V1
        && dynamic_meta.open_dynamic_v2.is_some()
    {
        return Err("schema v1 cannot carry open_dynamic_v2 payload");
    }

    match policy.mode {
        CompatibilityMode::StrictCurrent => {
            if static_meta.metadata_schema_version != CURRENT_METADATA_SCHEMA {
                return Err("strict mode only accepts current metadata schema version");
            }
        }
        CompatibilityMode::BackwardCompatible => {
            if static_meta.metadata_schema_version != METADATA_SCHEMA_V1
                && static_meta.metadata_schema_version != METADATA_SCHEMA_V2
            {
                return Err("unsupported metadata schema version");
            }
        }
    }

    if policy.write_schema_version < METADATA_SCHEMA_V1
        || policy.write_schema_version > CURRENT_METADATA_SCHEMA
    {
        return Err("write_schema_version is out of supported range");
    }

    // Mastering-tier files MUST NOT embed delivery-only adaptation policy.
    //
    // Rationale: mastering streams (`.qdrv64`, Float64 linear light) are the
    // creative source of truth. Adaptation policies describe how a *delivery*
    // player should adjust at runtime to its display, ambient conditions, or
    // frame-time budget — none of which exist on the mastering side. Carrying
    // these fields in a mastering file would either:
    //   1. mislead operators about which pipeline stage owns the policy, or
    //   2. silently disappear when the file is transcoded to delivery (the
    //      transcoder builds a fresh v2 payload for the delivery output).
    //
    // The four delivery-only fields gated here are:
    //   - `DynamicMeta::inverse_tone_mapping_hint` — SDR→HDR reconstruction
    //     hints consumed by `qdrv-decode::reconstruct` on the playback side.
    //     Mastering pixels are already linear-light floating-point;
    //     reconstruction is not applicable.
    //   - `OpenDynamicMetadataV2::adaptation_layer` — `DisplayAdaptationLayer`
    //     describes per-target-display-class roll-off and shadow lift, used
    //     by the delivery tone mapper.
    //   - `OpenDynamicMetadataV2::ambient_policy` — `AmbientAdaptivePolicy`
    //     describes lux-driven brightness boosting at playback.
    //   - `OpenDynamicMetadataV2::gaming_profile` — `GamingProfile` carries
    //     the per-frame budget and damping for low-latency playback.
    //   - `OpenDynamicMetadataV2::inverse_tone_mapping_hint` — same
    //     reconstruction concern as the top-level field, scoped to v2.
    //
    // The companion creative-intent fields on `OpenDynamicMetadataV2`
    // (`scene_constraints`, `object_constraints`, `temporal`,
    // `local_tone_map_grid`) ARE allowed on mastering streams: they describe
    // creative intent that survives transcode and is reused on the delivery
    // side. Only the *adaptation* fields are gated.
    if static_meta.tier == Tier::Mastering {
        if dynamic_meta.inverse_tone_mapping_hint.is_some() {
            return Err(
                "mastering-tier files must not carry inverse_tone_mapping_hint (delivery-only adaptation)",
            );
        }
        if let Some(v2) = &dynamic_meta.open_dynamic_v2 {
            if v2.adaptation_layer.is_some() {
                return Err(
                    "mastering-tier files must not carry open_dynamic_v2.adaptation_layer (delivery-only adaptation)",
                );
            }
            if v2.ambient_policy.is_some() {
                return Err(
                    "mastering-tier files must not carry open_dynamic_v2.ambient_policy (delivery-only adaptation)",
                );
            }
            if v2.gaming_profile.is_some() {
                return Err(
                    "mastering-tier files must not carry open_dynamic_v2.gaming_profile (delivery-only adaptation)",
                );
            }
            if v2.inverse_tone_mapping_hint.is_some() {
                return Err(
                    "mastering-tier files must not carry open_dynamic_v2.inverse_tone_mapping_hint (delivery-only adaptation)",
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_dynamic_v2::{
        AmbientAdaptivePolicy, DisplayAdaptationLayer, DisplayModelClass, GamingProfile,
        InverseToneMappingHint, OpenDynamicMetadataV2,
    };

    /// Builds a minimal-but-valid v2 payload (no delivery-only adaptation
    /// fields set) so each per-field test can flip exactly one field on.
    fn empty_v2() -> OpenDynamicMetadataV2 {
        OpenDynamicMetadataV2 {
            scene_constraints: Vec::new(),
            object_constraints: Vec::new(),
            temporal: Default::default(),
            local_tone_map_grid: None,
            adaptation_layer: None,
            ambient_policy: None,
            gaming_profile: None,
            inverse_tone_mapping_hint: None,
            spherical_projection: None,
        }
    }

    /// Returns a mastering-tier static+dynamic pair on schema v2 with an
    /// empty v2 payload — the canonical "valid mastering+v2" baseline.
    fn mastering_v2_pair() -> (StaticMeta, DynamicMeta) {
        let mut static_meta = StaticMeta::default_mastering();
        static_meta.metadata_schema_version = METADATA_SCHEMA_V2;
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.metadata_schema_version = METADATA_SCHEMA_V2;
        dynamic.open_dynamic_v2 = Some(empty_v2());
        (static_meta, dynamic)
    }

    #[test]
    fn compatibility_accepts_v1_in_backward_mode() {
        let mut static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        static_meta.metadata_schema_version = METADATA_SCHEMA_V1;
        dynamic.metadata_schema_version = METADATA_SCHEMA_V1;
        assert!(
            validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default()).is_ok()
        );
    }

    #[test]
    fn compatibility_requires_v2_payload_for_v2_schema() {
        let mut static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        static_meta.metadata_schema_version = METADATA_SCHEMA_V2;
        dynamic.metadata_schema_version = METADATA_SCHEMA_V2;
        assert!(
            validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default()).is_err()
        );
        dynamic.open_dynamic_v2 = Some(empty_v2());
        assert!(
            validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default()).is_ok()
        );
    }

    #[test]
    fn mastering_baseline_with_empty_v2_is_accepted() {
        let (static_meta, dynamic) = mastering_v2_pair();
        validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect("empty v2 on mastering tier must be accepted");
    }

    #[test]
    fn mastering_rejects_top_level_inverse_tone_mapping_hint() {
        let (static_meta, mut dynamic) = mastering_v2_pair();
        dynamic.inverse_tone_mapping_hint = Some(InverseToneMappingHint::default());
        let err = validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect_err("top-level inverse_tone_mapping_hint must be rejected on mastering");
        assert!(err.contains("inverse_tone_mapping_hint"), "got: {err}");
    }

    #[test]
    fn mastering_rejects_v2_adaptation_layer() {
        let (static_meta, mut dynamic) = mastering_v2_pair();
        if let Some(v2) = dynamic.open_dynamic_v2.as_mut() {
            v2.adaptation_layer = Some(DisplayAdaptationLayer {
                source_mastering_peak_nits: 1000.0,
                abstract_display_peak_nits: 600.0,
                display_model: DisplayModelClass::Oled,
                highlight_rolloff_strength: 0.5,
                shadow_lift_strength: 0.2,
            });
        }
        let err = validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect_err("adaptation_layer on mastering must be rejected");
        assert!(err.contains("adaptation_layer"), "got: {err}");
    }

    #[test]
    fn mastering_rejects_v2_ambient_policy() {
        let (static_meta, mut dynamic) = mastering_v2_pair();
        if let Some(v2) = dynamic.open_dynamic_v2.as_mut() {
            v2.ambient_policy = Some(AmbientAdaptivePolicy {
                lux_breakpoints: vec![50.0, 500.0],
                boost_multipliers: vec![1.0, 1.3],
                max_delta_per_second: 0.1,
            });
        }
        let err = validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect_err("ambient_policy on mastering must be rejected");
        assert!(err.contains("ambient_policy"), "got: {err}");
    }

    #[test]
    fn mastering_rejects_v2_gaming_profile() {
        let (static_meta, mut dynamic) = mastering_v2_pair();
        if let Some(v2) = dynamic.open_dynamic_v2.as_mut() {
            v2.gaming_profile = Some(GamingProfile::default());
        }
        let err = validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect_err("gaming_profile on mastering must be rejected");
        assert!(err.contains("gaming_profile"), "got: {err}");
    }

    #[test]
    fn mastering_rejects_v2_inverse_tone_mapping_hint() {
        let (static_meta, mut dynamic) = mastering_v2_pair();
        if let Some(v2) = dynamic.open_dynamic_v2.as_mut() {
            v2.inverse_tone_mapping_hint = Some(InverseToneMappingHint::default());
        }
        let err = validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect_err("v2.inverse_tone_mapping_hint on mastering must be rejected");
        assert!(
            err.contains("open_dynamic_v2.inverse_tone_mapping_hint"),
            "got: {err}"
        );
    }

    #[test]
    fn delivery_accepts_all_v2_adaptation_fields() {
        // Counterpart to the mastering rejection tests: every adaptation field
        // remains valid on the delivery side. Guards against the new gate
        // accidentally also rejecting delivery-tier streams.
        let mut static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        static_meta.metadata_schema_version = METADATA_SCHEMA_V2;
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.metadata_schema_version = METADATA_SCHEMA_V2;
        let mut v2 = empty_v2();
        v2.adaptation_layer = Some(DisplayAdaptationLayer {
            source_mastering_peak_nits: 1000.0,
            abstract_display_peak_nits: 600.0,
            display_model: DisplayModelClass::Oled,
            highlight_rolloff_strength: 0.5,
            shadow_lift_strength: 0.2,
        });
        v2.ambient_policy = Some(AmbientAdaptivePolicy {
            lux_breakpoints: vec![50.0, 500.0],
            boost_multipliers: vec![1.0, 1.3],
            max_delta_per_second: 0.1,
        });
        v2.gaming_profile = Some(GamingProfile::default());
        v2.inverse_tone_mapping_hint = Some(InverseToneMappingHint::default());
        dynamic.open_dynamic_v2 = Some(v2);
        dynamic.inverse_tone_mapping_hint = Some(InverseToneMappingHint::default());
        validate_compatibility(&static_meta, &dynamic, CompatibilityPolicy::default())
            .expect("delivery tier must accept all adaptation fields");
    }
}
