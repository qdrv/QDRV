// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-meta
//!
//! QDRV metadata types: static stream metadata and per-frame dynamic metadata.
//!
//! All structures are based on the **SMPTE ST 2094** dynamic metadata
//! framework, extended to IEEE 754 floating-point throughout. JSON
//! serialisation and deserialisation are provided via `serde` and
//! `serde_json`.
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`static_meta`] | [`StaticMeta`], [`MasteringDisplay`], [`ContentLightLevel`] |
//! | [`dynamic_meta`] | [`DynamicMeta`], [`DisplayHint`] |
//! | [`tone_curve`] | [`ToneMapCurve`], [`CurveAnchor`], [`CurveType`] |
//! | [`object_meta`] | [`BoundingBox`], [`MotionKeyframe`], [`ObjectRegion`], [`ObjectMeta`], [`RegionMotion`], [`SphericalRegion`], [`SphericalProjection`] |
//! | [`open_dynamic_v2`] | [`OpenDynamicMetadataV2`] and v2 policy structures |
//! | [`compatibility`] | Schema-version compatibility rules and `QDRV_FORMAT_VERSION` |
//! | [`manifest`] | [`SignedMetadataManifest`], HMAC-SHA256 sign/verify |
//! | [`hdr10plus`] | HDR10+ compatibility export (basic, advanced, adaptive, gaming) |
//! | [`interoperability`] | Dolby Vision compatibility sidecars and interop loss models |
//! | [`fidelity_contract`] | [`FidelityContract`], thresholds, and measurement results |
//! | [`binary`] | Legacy v1 binary metadata codec |
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 (GPLv2).

pub mod binary;
pub mod compatibility;
pub mod dynamic_meta;
pub mod fidelity_contract;
pub mod hdr10plus;
pub mod interoperability;
pub mod manifest;
pub mod object_meta;
pub mod open_dynamic_v2;
pub mod static_meta;
pub mod tone_curve;

pub use dynamic_meta::{DisplayHint, DynamicMeta};
pub use fidelity_contract::{FidelityContract, FidelityContractResult, MeasuredFidelity};
pub use interoperability::{
    DolbyVisionCompatibilityMetadata, DolbyVisionCompatibilityMode, DolbyVisionCompatibleSidecar,
    InteropLossReport, InteropTarget,
};
pub use manifest::{
    ManifestError, SignedMetadataManifest, sha256_hex, sign_manifest, verify_manifest,
};
pub use object_meta::{
    BoundingBox, MotionKeyframe, ObjectMeta, ObjectRegion, RegionMotion, SphericalProjection,
    SphericalRegion,
};
pub use open_dynamic_v2::{
    AmbientAdaptivePolicy, DisplayAdaptationLayer, DisplayModelClass, GamingProfile,
    InverseToneMappingHint, LocalToneMapCell, LocalToneMapGrid, ObjectConstraint,
    OpenDynamicMetadataV2, SceneConstraint, TemporalConstraint,
};
pub use static_meta::{
    ChromaSubsampling, ChromaticityPoint, ContentLightLevel, MasteringDisplay, Precision,
    StaticMeta, Tier,
};
pub use tone_curve::{CurveAnchor, CurveType, ToneMapCurve};

use thiserror::Error;

/// Errors produced by `qdrv-meta` serialisation and deserialisation operations.
#[derive(Debug, Error)]
pub enum MetaError {
    /// A JSON serialisation or deserialisation error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// An I/O error encountered during metadata read or write.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Validation error for strict schema checking.
    #[error("validation error: {0}")]
    Validation(String),
}

/// Serialises a metadata value to a pretty-printed JSON string.
///
/// # Errors
/// Returns [`MetaError::Json`] if the value cannot be serialised.
pub fn to_json<T: serde::Serialize>(value: &T) -> Result<String, MetaError> {
    Ok(serde_json::to_string_pretty(value)?)
}

/// Deserialises a metadata value from a JSON string.
///
/// # Errors
/// Returns [`MetaError::Json`] if the string is not valid JSON or does not
/// match the expected structure.
pub fn from_json<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, MetaError> {
    Ok(serde_json::from_str(json)?)
}

/// Deserialises and strictly validates [`StaticMeta`].
pub fn from_json_strict_static(json: &str) -> Result<StaticMeta, MetaError> {
    let meta: StaticMeta = serde_json::from_str(json)?;
    meta.validate()
        .map_err(|e| MetaError::Validation(e.to_string()))?;
    Ok(meta)
}

/// Deserialises and strictly validates [`DynamicMeta`].
pub fn from_json_strict_dynamic(json: &str) -> Result<DynamicMeta, MetaError> {
    let meta: DynamicMeta = serde_json::from_str(json)?;
    meta.validate()
        .map_err(|e| MetaError::Validation(e.to_string()))?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises `to_json` / `from_json` on `StaticMeta`, ensuring a lossless round-trip for the default delivery fixture.
    #[test]
    fn test_static_meta_json_roundtrip() {
        // Serialising and then deserialising static metadata must recover the
        // original value exactly.
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let json = to_json(&meta).unwrap();
        let recovered: StaticMeta = from_json(&json).unwrap();
        assert_eq!(meta, recovered);
    }

    /// Exercises JSON serialisation for `DynamicMeta`, including tone-map anchors, and checks exact recovery.
    #[test]
    fn test_dynamic_meta_json_roundtrip() {
        // Serialising and then deserialising dynamic metadata must recover the
        // original value exactly.
        let meta = DynamicMeta::new(42, 1200.0, 180.0);
        let json = to_json(&meta).unwrap();
        let recovered: DynamicMeta = from_json(&json).unwrap();
        assert_eq!(meta, recovered);
    }

    /// Asserts that `StaticMeta::default_delivery` sets the normative colour-space and metadata-standard string fields.
    #[test]
    fn test_static_meta_conformant_fields() {
        // A delivery-tier static metadata block must contain the mandatory
        // conformant field values specified by the QDRV format.
        let meta = StaticMeta::default_delivery(800.0, 300.0);
        assert_eq!(meta.colour_standard, "rec2100");
        assert_eq!(meta.colour_primaries, "rec2020");
        assert_eq!(meta.transfer_function, "st2084_pq");
        assert_eq!(meta.dynamic_metadata_standard, "st2094");
    }
}
