// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Static (per-stream) QDRV metadata.
//!
//! Static metadata describes the signal characteristics that remain constant
//! for the entire duration of a QDRV stream. It is carried once per stream,
//! typically in the container header.
//!
//! The mastering display colour volume fields extend SMPTE ST 2086 into the
//! Float64 domain.

use qdrv_core::colors::primaries;
use serde::{Deserialize, Serialize};

use crate::compatibility::{METADATA_SCHEMA_V1, METADATA_SCHEMA_V2, QDRV_FORMAT_VERSION};

/// A CIE 1931 xy chromaticity coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromaticityPoint {
    /// x chromaticity coordinate.
    pub x: f64,
    /// y chromaticity coordinate.
    pub y: f64,
}

/// Mastering display colour volume metadata per SMPTE ST 2086.
///
/// Describes the colour volume of the display on which the content was
/// graded. All chromaticity values are in CIE 1931 xy. Luminance values
/// are in cd/m² (nits).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MasteringDisplay {
    /// Red primary chromaticity.
    pub red_primary: ChromaticityPoint,
    /// Green primary chromaticity.
    pub green_primary: ChromaticityPoint,
    /// Blue primary chromaticity.
    pub blue_primary: ChromaticityPoint,
    /// White point chromaticity.
    pub white_point: ChromaticityPoint,
    /// Minimum mastering display luminance in nits.
    pub min_luminance_nits: f64,
    /// Maximum mastering display luminance in nits.
    pub max_luminance_nits: f64,
}

impl Default for MasteringDisplay {
    /// Returns the standard ITU-R Rec. 2100 / Rec. 2020 primaries with the
    /// D65 white point and a luminance range of `[0.0, 10 000.0]` nits.
    ///
    /// Chromaticities are pulled directly from
    /// [`qdrv_core::colors::primaries`] so the mastering display defaults
    /// cannot drift from the centralised Rec. 2020 primary definitions
    /// used elsewhere in the workspace.
    fn default() -> Self {
        Self {
            red_primary: ChromaticityPoint {
                x: primaries::RED.0,
                y: primaries::RED.1,
            },
            green_primary: ChromaticityPoint {
                x: primaries::GREEN.0,
                y: primaries::GREEN.1,
            },
            blue_primary: ChromaticityPoint {
                x: primaries::BLUE.0,
                y: primaries::BLUE.1,
            },
            white_point: ChromaticityPoint {
                x: primaries::WHITE.0,
                y: primaries::WHITE.1,
            },
            min_luminance_nits: 0.0,
            max_luminance_nits: 10_000.0,
        }
    }
}

/// Content light level metadata.
///
/// Carries the peak and average light level statistics for the entire stream,
/// allowing decoders to perform initial display-capability matching before
/// per-frame dynamic metadata is processed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentLightLevel {
    /// Maximum content light level across all frames, in nits.
    pub max_cll_nits: f32,
    /// Maximum frame-average light level across all frames, in nits.
    pub max_fall_nits: f32,
}

/// The QDRV processing tier of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Mastering/archival tier: Float64 linear light, unbounded luminance.
    Mastering,
    /// Delivery tier: Float32 SMPTE ST 2084 PQ-encoded, 0–10 000 nits.
    Delivery,
}

/// The IEEE 754 floating-point precision used for pixel data in a stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Precision {
    /// 64-bit double precision. Used for the mastering/archival tier.
    Float64,
    /// 32-bit single precision. Used for the delivery tier.
    Float32,
}

/// The chroma subsampling mode used in a QDRV delivery-tier stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ChromaSubsampling {
    /// 4:4:4 full chroma resolution. The default for QDRV streams.
    #[serde(rename = "4:4:4")]
    Yuv444,
    /// 4:2:0 half chroma resolution. Permitted for bandwidth-constrained
    /// delivery only; the mastering tier always uses 4:4:4.
    #[serde(rename = "4:2:0")]
    Yuv420,
}

/// Static (per-stream) QDRV metadata.
///
/// This structure is serialised to JSON and embedded once in the stream
/// container. It describes all signal characteristics that remain constant
/// for the duration of the stream, including the colour standard, transfer
/// function, precision, and mastering display colour volume.
///
/// All QDRV-conformant streams must set:
/// - `colour_standard` to `"rec2100"`
/// - `colour_primaries` to `"rec2020"`
/// - `transfer_function` to `"st2084_pq"` (delivery) or `"linear"` (mastering)
/// - `dynamic_metadata_standard` to `"st2094"`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticMeta {
    /// QDRV specification version string (e.g., `"0.1.0"`).
    pub qdrv_version: String,
    /// Metadata schema version.
    #[serde(default = "default_metadata_schema_version")]
    pub metadata_schema_version: u16,
    /// Processing tier of this stream.
    pub tier: Tier,
    /// Pixel data precision for this stream.
    pub precision: Precision,
    /// Colour standard. Must be `"rec2100"` for conformant QDRV streams.
    pub colour_standard: String,
    /// Colour primaries. Must be `"rec2020"` for conformant QDRV streams,
    /// as these primaries are inherited by ITU-R Rec. 2100.
    pub colour_primaries: String,
    /// Transfer function. Delivery tier: `"st2084_pq"`.
    /// Mastering tier: `"linear"`.
    pub transfer_function: String,
    /// Dynamic metadata standard. Must be `"st2094"` for conformant QDRV streams.
    pub dynamic_metadata_standard: String,
    /// Chroma subsampling mode.
    pub chroma_subsampling: ChromaSubsampling,
    /// Mastering display colour volume per SMPTE ST 2086.
    pub mastering_display: MasteringDisplay,
    /// Stream-level content light level statistics.
    pub content_light_level: ContentLightLevel,
    /// Optional compatibility tags for migration and audits.
    #[serde(default)]
    pub compatibility_tags: Vec<String>,
}

impl StaticMeta {
    /// Creates a default static metadata block for a QDRV delivery-tier stream.
    ///
    /// Sets all mandatory fields to their conformant QDRV values:
    /// ITU-R Rec. 2100, SMPTE ST 2084 PQ, SMPTE ST 2094, Float32, 4:4:4.
    ///
    /// # Arguments
    /// * `max_cll_nits`  — Maximum content light level for the stream, in nits.
    /// * `max_fall_nits` — Maximum frame-average light level for the stream, in nits.
    pub fn default_delivery(max_cll_nits: f32, max_fall_nits: f32) -> Self {
        Self {
            qdrv_version: QDRV_FORMAT_VERSION.to_string(),
            metadata_schema_version: METADATA_SCHEMA_V1,
            tier: Tier::Delivery,
            precision: Precision::Float32,
            colour_standard: "rec2100".to_string(),
            colour_primaries: "rec2020".to_string(),
            transfer_function: "st2084_pq".to_string(),
            dynamic_metadata_standard: "st2094".to_string(),
            chroma_subsampling: ChromaSubsampling::Yuv444,
            mastering_display: MasteringDisplay::default(),
            content_light_level: ContentLightLevel {
                max_cll_nits,
                max_fall_nits,
            },
            compatibility_tags: vec!["backward_v1_read".to_string()],
        }
    }

    /// Creates a default static metadata block for a QDRV mastering-tier stream.
    ///
    /// Sets all mandatory fields to their conformant QDRV values:
    /// ITU-R Rec. 2100, linear transfer function, SMPTE ST 2094, Float64, 4:4:4.
    /// The luminance range is set to the full 10 000-nit PQ ceiling, though the
    /// mastering tier itself imposes no upper bound on stored luminance values.
    pub fn default_mastering() -> Self {
        Self {
            qdrv_version: QDRV_FORMAT_VERSION.to_string(),
            metadata_schema_version: METADATA_SCHEMA_V1,
            tier: Tier::Mastering,
            precision: Precision::Float64,
            colour_standard: "rec2100".to_string(),
            colour_primaries: "rec2020".to_string(),
            transfer_function: "linear".to_string(),
            dynamic_metadata_standard: "st2094".to_string(),
            chroma_subsampling: ChromaSubsampling::Yuv444,
            mastering_display: MasteringDisplay::default(),
            content_light_level: ContentLightLevel {
                max_cll_nits: 10_000.0,
                max_fall_nits: 10_000.0,
            },
            compatibility_tags: vec!["backward_v1_read".to_string()],
        }
    }

    /// Validates static metadata invariants required by the QDRV format.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.qdrv_version.trim().is_empty() {
            return Err("qdrv_version must not be empty");
        }
        if self.metadata_schema_version < METADATA_SCHEMA_V1
            || self.metadata_schema_version > METADATA_SCHEMA_V2
        {
            return Err("metadata_schema_version is unsupported");
        }

        match (self.tier, self.precision) {
            (Tier::Delivery, Precision::Float32) | (Tier::Mastering, Precision::Float64) => {}
            _ => return Err("tier and precision combination is invalid"),
        }

        if self.colour_standard != "rec2100" {
            return Err("colour_standard must be \"rec2100\"");
        }
        if self.colour_primaries != "rec2020" {
            return Err("colour_primaries must be \"rec2020\"");
        }
        match self.tier {
            Tier::Delivery if self.transfer_function != "st2084_pq" => {
                return Err("delivery tier requires transfer_function \"st2084_pq\"");
            }
            Tier::Mastering if self.transfer_function != "linear" => {
                return Err("mastering tier requires transfer_function \"linear\"");
            }
            _ => {}
        }
        if self.dynamic_metadata_standard != "st2094" {
            return Err("dynamic_metadata_standard must be \"st2094\"");
        }

        let md = &self.mastering_display;
        let points = [
            md.red_primary,
            md.green_primary,
            md.blue_primary,
            md.white_point,
        ];
        for p in points {
            if !p.x.is_finite() || !p.y.is_finite() {
                return Err("mastering display chromaticity points must be finite");
            }
            if !(0.0..=1.0).contains(&p.x) || !(0.0..=1.0).contains(&p.y) {
                return Err("mastering display chromaticity points must be in [0.0, 1.0]");
            }
        }
        if !md.min_luminance_nits.is_finite() || !md.max_luminance_nits.is_finite() {
            return Err("mastering display luminance values must be finite");
        }
        if md.min_luminance_nits < 0.0 || md.max_luminance_nits <= md.min_luminance_nits {
            return Err("mastering display requires max_luminance_nits > min_luminance_nits >= 0");
        }

        let cll = self.content_light_level;
        if !cll.max_cll_nits.is_finite() || !cll.max_fall_nits.is_finite() {
            return Err("content light level values must be finite");
        }
        if cll.max_cll_nits < 0.0 || cll.max_fall_nits < 0.0 {
            return Err("content light level values must be non-negative");
        }
        if cll.max_fall_nits > cll.max_cll_nits {
            return Err("max_fall_nits must be <= max_cll_nits");
        }

        for tag in &self.compatibility_tags {
            if tag.trim().is_empty() {
                return Err("compatibility_tags cannot contain empty entries");
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
    fn test_validate_accepts_default_delivery() {
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        assert!(meta.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_invalid_tier_precision_pair() {
        let mut meta = StaticMeta::default_delivery(1000.0, 400.0);
        meta.precision = Precision::Float64;
        assert!(meta.validate().is_err());
    }

    /// B-5 follow-up: the symmetric case — mastering tier paired with
    /// Float32 — must also be rejected by `validate`. Only
    /// `Mastering+Float64` and `Delivery+Float32` are valid combinations.
    #[test]
    fn test_validate_rejects_mastering_with_float32() {
        let mut meta = StaticMeta::default_mastering();
        meta.precision = Precision::Float32;
        let err = meta.validate().unwrap_err();
        assert!(
            err.contains("tier and precision"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_rejects_invalid_transfer_for_tier() {
        let mut meta = StaticMeta::default_mastering();
        meta.transfer_function = "st2084_pq".to_string();
        assert!(meta.validate().is_err());
    }
}
