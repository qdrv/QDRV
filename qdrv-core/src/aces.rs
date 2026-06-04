// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! ACES (Academy Colour Encoding System) colour space transforms.
//!
//! Provides conversion matrices between ACES colour spaces and ITU-R Rec. 2020,
//! enabling QDRV to ingest content from ACES-based production pipelines and
//! export to ACES archival formats.
//!
//! ## Supported colour spaces
//!
//! | Colour space | Primaries | White point | Usage |
//! |-------------|-----------|-------------|-------|
//! | ACES2065-1 (AP0) | ACES primaries 0 | ACES white (approx. D60) | Archival interchange |
//! | ACEScg (AP1) | ACES primaries 1 | ACES white (approx. D60) | Scene-referred compositing |
//!
//! ## Chromatic adaptation
//!
//! ACES uses a white point at approximately D60 (x=0.32168, y=0.33767), while
//! QDRV/Rec. 2020 uses D65 (x=0.3127, y=0.3290). The matrices below include
//! a Bradford chromatic adaptation transform between D60 and D65, so a single
//! matrix multiply handles both the primary transform and the white point shift.

use crate::colors::apply_matrix;

/// ACES AP0 (ACES2065-1) primaries in CIE 1931 xy chromaticity.
pub mod ap0_primaries {
    /// CIE 1931 *x*, *y* chromaticity coordinates of the AP0 red primary for ACES2065-1.
    ///
    /// This red lies well outside typical display gamuts (for example, Rec. 709 and DCI-P3),
    /// toward the spectral locus. That placement yields an extremely wide, scene-referred
    /// encoding volume suited to archival interchange, at the cost of many real-world
    /// colours requiring negative *RGB* coefficients when expressed in AP0. The values
    /// here are the canonical SMPTE ST 2065-1 / ACES specification figures; downstream
    /// matrices in this module assume these exact chromaticities when converting to
    /// ITU-R Rec. 2020 with Bradford chromatic adaptation.
    pub const RED: (f64, f64) = (0.7347, 0.2653);
    /// CIE 1931 *x*, *y* chromaticity coordinates of the AP0 green primary for ACES2065-1.
    ///
    /// The green primary is placed at *y* = 1.0 on the CIE 1931 diagram, which is a
    /// mathematical boundary rather than a physically realisable narrow-band emitter at
    /// a single wavelength. In practice, this choice maximises the encoded triangle’s
    /// extent in *xy* and helps preserve highly saturated greens through long production
    /// chains. Matrices that transform AP0 to other colour spaces treat this coordinate
    /// pair as a precise definition of the AP0 green corner, independent of how one
    /// would realise it on a physical display.
    pub const GREEN: (f64, f64) = (0.0000, 1.0000);
    /// CIE 1931 *x*, *y* chromaticity coordinates of the AP0 blue primary for ACES2065-1.
    ///
    /// The blue primary sits below the spectral locus line in *y*, which is admissible
    /// for a virtual RGB basis used in computation: the resulting triangle still
    /// encloses a large portion of visible colours when combined with the red and green
    /// primaries above. Consumers should treat these numbers as normative constants for
    /// matrix derivation, not as a recipe for building a literal three-primary projector.
    pub const BLUE: (f64, f64) = (0.0001, -0.0770);
    /// CIE 1931 *x*, *y* chromaticity coordinates of the ACES white point (approximately CIE
    /// illuminant D60), shared by AP0 and AP1 in the ACES system.
    ///
    /// This white is close to, but not identical to, the D65 white point used by
    /// ITU-R Rec. 2020 / Rec. 2100 (*x* = 0.3127, *y* = 0.3290). For that reason, the
    /// conversion matrices in this module embed a Bradford chromatic adaptation between
    /// D60 and D65, so a single matrix multiply performs both the primary transform and the
    /// white-point alignment. Using this constant as the reference white when reasoning
    /// about AP0 *RGB* triplets keeps scene-referred grading decisions consistent with the
    /// published ACES documentation.
    pub const WHITE: (f64, f64) = (0.32168, 0.33767);
}

/// ACES AP1 (ACEScg) primaries in CIE 1931 xy chromaticity.
pub mod ap1_primaries {
    /// CIE 1931 *x*, *y* chromaticity coordinates of the AP1 red primary used in ACEScg.
    ///
    /// Compared with AP0, AP1 pulls the red primary inward toward colours that are more
    /// commonly encountered in digital compositing and real-time rendering. The AP1
    /// triangle remains wider than Rec. 709, overlaps much of DCI-P3, and was chosen so
    /// that typical CG and plate colours remain representable with moderate *RGB* values.
    /// These coordinates are the published AP1 basis against which the `ACES_AP1_TO_REC2020`
    /// and `REC2020_TO_ACES_AP1` matrices were derived (including D60→D65 adaptation).
    pub const RED: (f64, f64) = (0.713, 0.293);
    /// CIE 1931 *x*, *y* chromaticity coordinates of the AP1 green primary used in ACEScg.
    ///
    /// The green corner is placed to balance coverage of foliage, displays, and synthetic
    /// gradients used in visual-effects work. Together with the red and blue primaries,
    /// it defines a wide-but-practical working space that interoperates cleanly with
    /// modern HDR mastering pipelines once adapted to D65 via the matrices in this module.
    pub const GREEN: (f64, f64) = (0.165, 0.830);
    /// CIE 1931 *x*, *y* chromaticity coordinates of the AP1 blue primary used in ACEScg.
    ///
    /// This blue completes the AP1 gamut triangle used throughout ACEScg toolchains.
    /// When converting AP1 *RGB* to linear Rec. 2020, the blue axis’s orientation relative
    /// to Rec. 2020 primaries determines how deep violets and cyans are apportioned across
    /// channels after adaptation; the numeric pair here is therefore part of the normative
    /// definition of AP1, not an adjustable artistic parameter.
    pub const BLUE: (f64, f64) = (0.128, 0.044);
    /// CIE 1931 *x*, *y* chromaticity coordinates of the ACES white point (approximately CIE
    /// illuminant D60), identical in *xy* to the AP0 white point above.
    ///
    /// ACEScg shares the same system white as ACES2065-1, so compositors may move assets
    /// between AP0 archival containers and AP1 working buffers without a white-point
    /// discontinuity—only the primary basis changes. Rec. 2020 conversions in QDRV still
    /// apply Bradford adaptation from this D60-class white to Rec. 2020’s D65 anchor, which
    /// is why a neutral `(1.0, 1.0, 1.0)` *RGB* triplet in ACES does not, in general, map to
    /// an exact `(1.0, 1.0, 1.0)` triplet in Rec. 2020 unless numerically coincident within
    /// tolerance after adaptation and matrix rounding.
    pub const WHITE: (f64, f64) = (0.32168, 0.33767);
}

/// ACES AP0 (ACES2065-1) → linear Rec. 2020 RGB, with D60→D65 Bradford
/// chromatic adaptation included.
///
/// Source: derived from the ACES AP0→XYZ matrix (SMPTE ST 2065-1), Bradford
/// D60→D65 adaptation, and XYZ→Rec. 2020 matrix.
pub const ACES_AP0_TO_REC2020: [[f64; 3]; 3] = [
    [1.4904095, -0.2661709, -0.2242386],
    [-0.0801675, 1.1821671, -0.1019996],
    [0.0032276, -0.0347765, 1.0315488],
];

/// Linear Rec. 2020 RGB → ACES AP0 (ACES2065-1), with D65→D60 Bradford
/// chromatic adaptation included.
///
/// Coefficients are the numerical inverse of [`ACES_AP0_TO_REC2020`] so that
/// AP0 → Rec. 2020 → AP0 roundtrips stay within floating-point noise for
/// in-gamut scene values (the published ACES CTL matrices round slightly
/// differently from a strict matrix inverse).
pub const REC2020_TO_ACES_AP0: [[f64; 3]; 3] = [
    [0.6790856, 0.1577009, 0.1632135],
    [0.0460020, 0.8590547, 0.0949433],
    [-0.0005739, 0.0284678, 0.9721062],
];

/// ACES AP1 (ACEScg) → linear Rec. 2020 RGB, with D60→D65 Bradford
/// chromatic adaptation included.
pub const ACES_AP1_TO_REC2020: [[f64; 3]; 3] = [
    [1.0258247, -0.0200532, -0.0057716],
    [-0.0022344, 1.0045865, -0.0023521],
    [-0.0050134, -0.0252901, 1.0303034],
];

/// Linear Rec. 2020 RGB → ACES AP1 (ACEScg), with D65→D60 Bradford
/// chromatic adaptation included.
///
/// Coefficients are the numerical inverse of [`ACES_AP1_TO_REC2020`]; see
/// [`REC2020_TO_ACES_AP0`] for why QDRV uses an explicit inverse here.
pub const REC2020_TO_ACES_AP1: [[f64; 3]; 3] = [
    [0.9748950, 0.0195991, 0.0055059],
    [0.0021796, 0.9955355, 0.0022850],
    [0.0047972, 0.0245320, 0.9706707],
];

/// Converts linear ACES AP0 (ACES2065-1) RGB to linear Rec. 2020 RGB.
///
/// Includes Bradford chromatic adaptation from ACES white (~D60) to D65.
#[inline]
pub fn aces_ap0_to_rec2020(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_matrix(rgb, &ACES_AP0_TO_REC2020)
}

/// Converts linear Rec. 2020 RGB to linear ACES AP0 (ACES2065-1) RGB.
///
/// Includes Bradford chromatic adaptation from D65 to ACES white (~D60).
#[inline]
pub fn rec2020_to_aces_ap0(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_matrix(rgb, &REC2020_TO_ACES_AP0)
}

/// Converts linear ACES AP1 (ACEScg) RGB to linear Rec. 2020 RGB.
///
/// Includes Bradford chromatic adaptation from ACES white (~D60) to D65.
#[inline]
pub fn aces_ap1_to_rec2020(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_matrix(rgb, &ACES_AP1_TO_REC2020)
}

/// Converts linear Rec. 2020 RGB to linear ACES AP1 (ACEScg) RGB.
///
/// Includes Bradford chromatic adaptation from D65 to ACES white (~D60).
#[inline]
pub fn rec2020_to_aces_ap1(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_matrix(rgb, &REC2020_TO_ACES_AP1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that converting a sample triplet from ACES AP0 to linear Rec. 2020 and
    /// back with [`aces_ap0_to_rec2020`] and [`rec2020_to_aces_ap0`] recovers the original
    /// *RGB* values within a tight floating-point tolerance, so the published forward and
    /// inverse matrices remain near-perfect inverses for in-range scene-referred data.
    #[test]
    fn test_aces_ap0_roundtrip() {
        let colors = (0.8_f64, 0.5, 0.3);
        let rec2020 = aces_ap0_to_rec2020(colors);
        let back = rec2020_to_aces_ap0(rec2020);
        assert!((back.0 - colors.0).abs() < 1e-5, "R: {}", back.0);
        assert!((back.1 - colors.1).abs() < 1e-5, "G: {}", back.1);
        assert!((back.2 - colors.2).abs() < 1e-5, "B: {}", back.2);
    }

    /// Verifies the same near-inverse property as [`test_aces_ap0_roundtrip`], but for the
    /// ACEScg (AP1) basis using [`aces_ap1_to_rec2020`] and [`rec2020_to_aces_ap1`], ensuring
    /// compositing-space roundtrips through Rec. 2020 do not drift beyond the expected
    /// numerical noise of fixed-precision matrix coefficients.
    #[test]
    fn test_aces_ap1_roundtrip() {
        let colors = (0.6_f64, 0.4, 0.2);
        let rec2020 = aces_ap1_to_rec2020(colors);
        let back = rec2020_to_aces_ap1(rec2020);
        assert!((back.0 - colors.0).abs() < 1e-5, "R: {}", back.0);
        assert!((back.1 - colors.1).abs() < 1e-5, "G: {}", back.1);
        assert!((back.2 - colors.2).abs() < 1e-5, "B: {}", back.2);
    }

    /// Verifies that AP0 unit white `(1.0, 1.0, 1.0)` maps to linear Rec. 2020 coordinates
    /// within five percent of unity on every channel. ACES white (approximately D60) and
    /// Rec. 2020’s D65 anchor are close relatives, not identical, so an exact `(1, 1, 1)`
    /// match is neither required nor expected; the loose bound instead confirms that
    /// Bradford adaptation plus the AP0→Rec. 2020 matrix behaves sensibly for neutrals.
    #[test]
    fn test_aces_white_near_invariant() {
        // ACES white and D65 white are close but not identical, so a
        // (1,1,1) input will not map to exactly (1,1,1). However, the
        // result should be close — within 5% — confirming the chromatic
        // adaptation is behaving as intended.
        let white = (1.0_f64, 1.0, 1.0);
        let rec2020 = aces_ap0_to_rec2020(white);
        assert!((rec2020.0 - 1.0).abs() < 0.05, "R: {}", rec2020.0);
        assert!((rec2020.1 - 1.0).abs() < 0.05, "G: {}", rec2020.1);
        assert!((rec2020.2 - 1.0).abs() < 0.05, "B: {}", rec2020.2);
    }

    /// Absolute-reference guard. The round-trip tests above only prove that
    /// each forward/inverse pair is a mutual inverse — a property that holds
    /// for *any* invertible matrix pair, correct or not, and which previously
    /// let a colorimetrically wrong (near-identity) AP0 matrix pass unnoticed.
    /// These assertions instead pin the forward matrices to values derived
    /// from the published ACES primaries with Bradford D60→D65 adaptation, so
    /// a self-consistent-but-wrong matrix can no longer slip through.
    #[test]
    fn aces_forward_matrices_match_absolute_reference() {
        // AP0 is a far wider gamut than Rec. 2020, so its forward matrix must
        // carry strong off-diagonal terms: the AP0 red corner (1, 0, 0) maps
        // well outside the Rec. 2020 unit cube. (A near-identity matrix — the
        // original defect — would instead map it to ≈(1, 0, 0).)
        let red = aces_ap0_to_rec2020((1.0, 0.0, 0.0));
        assert!((red.0 - 1.4904095).abs() < 1e-4, "AP0→Rec2020 R: {}", red.0);
        assert!((red.1 + 0.0801675).abs() < 1e-4, "AP0→Rec2020 G: {}", red.1);
        assert!((red.2 - 0.0032276).abs() < 1e-4, "AP0→Rec2020 B: {}", red.2);

        // AP1 (ACEScg) must match the canonical published ACEScg→Rec. 2020
        // matrix to within rounding of the stored 7-digit coefficients.
        let red = aces_ap1_to_rec2020((1.0, 0.0, 0.0));
        assert!((red.0 - 1.0258247).abs() < 1e-4, "AP1→Rec2020 R: {}", red.0);
        assert!((red.1 + 0.0022344).abs() < 1e-4, "AP1→Rec2020 G: {}", red.1);
        assert!((red.2 + 0.0050134).abs() < 1e-4, "AP1→Rec2020 B: {}", red.2);
    }
}
