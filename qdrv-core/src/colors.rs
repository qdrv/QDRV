// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! ITU-R Rec. 2100 / Rec. 2020 colour primaries, matrices, and gamut transforms.
//!
//! QDRV uses ITU-R Rec. 2100 (BT.2100) as its primary colour standard, which
//! inherits the wide-gamut primaries of ITU-R Rec. 2020 (BT.2020). These
//! primaries are shared by every modern dynamic-range format — integer
//! HDR (HDR10, HDR10+), proprietary Dolby Vision, and the floating-point
//! QDRV successor — ensuring colour-primary compatibility across the
//! whole ecosystem.
//!
//! All colour matrix operations are performed in Float64 to match the
//! precision requirements of the mastering tier.

/// ITU-R Rec. 2100 / Rec. 2020 colour primaries in CIE 1931 xy chromaticity.
/// These coordinates are identical for both standards and are the primaries
/// used by every modern HDR format.
pub mod primaries {
    /// Red primary xy chromaticity.
    pub const RED: (f64, f64) = (0.708, 0.292);
    /// Green primary xy chromaticity.
    pub const GREEN: (f64, f64) = (0.170, 0.797);
    /// Blue primary xy chromaticity.
    pub const BLUE: (f64, f64) = (0.131, 0.046);
    /// D65 white point xy chromaticity, as used by Rec. 2100 and Rec. 2020.
    pub const WHITE: (f64, f64) = (0.3127, 0.3290);
}

/// ITU-R Rec. 2100 non-constant luminance (NCL) luma coefficients.
/// These define how each RGB channel contributes to the luma signal Y.
pub mod ncl {
    /// Luma coefficient for the red channel.
    pub const KR: f64 = 0.2627;
    /// Luma coefficient for the green channel.
    pub const KG: f64 = 0.6780;
    /// Luma coefficient for the blue channel.
    pub const KB: f64 = 0.0593;
}

// ---------------------------------------------------------------------------
// Colour matrices
// Source: ITU-R BT.2020 Table 4 and derived inverse matrices.
// ---------------------------------------------------------------------------

/// Linear Rec. 2020 RGB → CIE XYZ (D65) colour matrix.
/// Source: ITU-R BT.2020 Table 4.
pub const REC2020_TO_XYZ: [[f64; 3]; 3] = [
    [0.6369580, 0.1446169, 0.1688810],
    [0.2627002, 0.6779981, 0.0593017],
    [0.0000000, 0.0280727, 1.0609851],
];

/// CIE XYZ (D65) → linear Rec. 2020 RGB colour matrix.
/// Inverse of [`REC2020_TO_XYZ`].
pub const XYZ_TO_REC2020: [[f64; 3]; 3] = [
    [1.7166512, -0.3556708, -0.2533663],
    [-0.6666844, 1.6164812, 0.0157685],
    [0.0176399, -0.0427706, 0.9421031],
];

/// Linear Rec. 709 (sRGB primaries) RGB → CIE XYZ (D65) colour matrix.
pub const REC709_TO_XYZ: [[f64; 3]; 3] = [
    [0.4123908, 0.3575843, 0.1804808],
    [0.2126390, 0.7151687, 0.0721923],
    [0.0193308, 0.1191948, 0.9505322],
];

/// CIE XYZ (D65) → linear Rec. 709 (sRGB primaries) colour matrix.
/// Inverse of [`REC709_TO_XYZ`].
pub const XYZ_TO_REC709: [[f64; 3]; 3] = [
    [3.2409699, -1.5373832, -0.4986108],
    [-0.9692436, 1.8759675, 0.0415551],
    [0.0556301, -0.2039770, 1.0569715],
];

// ---------------------------------------------------------------------------
// Matrix operations
// ---------------------------------------------------------------------------

/// Applies a 3×3 colour matrix to an RGB triplet in Float64.
///
/// Computes `out = M × [r, g, b]ᵀ`. This is the fundamental building block
/// for all colour space conversions in QDRV.
#[inline]
pub fn apply_matrix(rgb: (f64, f64, f64), m: &[[f64; 3]; 3]) -> (f64, f64, f64) {
    (
        m[0][0] * rgb.0 + m[0][1] * rgb.1 + m[0][2] * rgb.2,
        m[1][0] * rgb.0 + m[1][1] * rgb.1 + m[1][2] * rgb.2,
        m[2][0] * rgb.0 + m[2][1] * rgb.1 + m[2][2] * rgb.2,
    )
}

// ---------------------------------------------------------------------------
// Gamut conversions
// ---------------------------------------------------------------------------

/// Converts linear Rec. 709 (sRGB primaries) RGB to linear Rec. 2020 RGB.
///
/// Uses CIE XYZ (D65) as an intermediate colour space. All input values must
/// be linear light with no gamma or PQ encoding applied.
#[inline]
pub fn rec709_to_rec2020(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    let xyz = apply_matrix(rgb, &REC709_TO_XYZ);
    apply_matrix(xyz, &XYZ_TO_REC2020)
}

/// Converts linear Rec. 2020 RGB to linear Rec. 709 (sRGB primaries) RGB.
///
/// Uses CIE XYZ (D65) as an intermediate colour space. Output values outside
/// `[0.0, 1.0]` indicate colours that are outside the Rec. 709 gamut and
/// cannot be represented without clipping or gamut compression.
#[inline]
pub fn rec2020_to_rec709(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    let xyz = apply_matrix(rgb, &REC2020_TO_XYZ);
    apply_matrix(xyz, &XYZ_TO_REC709)
}

/// Converts linear Rec. 2020 RGB to CIE XYZ (D65).
#[inline]
pub fn rec2020_to_xyz(rgb: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_matrix(rgb, &REC2020_TO_XYZ)
}

/// Converts CIE XYZ (D65) to linear Rec. 2020 RGB.
#[inline]
pub fn xyz_to_rec2020(xyz: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_matrix(xyz, &XYZ_TO_REC2020)
}

// ---------------------------------------------------------------------------
// sRGB transfer function (IEC 61966-2-1)
// ---------------------------------------------------------------------------

/// Applies the sRGB forward transfer function (linear → sRGB gamma).
///
/// The piecewise function is defined in IEC 61966-2-1:
/// - `c <= 0.0031308`: `out = 12.92 * c`
/// - `c >  0.0031308`: `out = 1.055 * c^(1/2.4) - 0.055`
///
/// Input should be linear light in `[0.0, 1.0]`. Values outside this range
/// are clamped before conversion.
#[inline]
pub fn linear_to_srgb(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Applies the sRGB inverse transfer function (sRGB gamma → linear).
///
/// The piecewise function is the inverse of [`linear_to_srgb`]:
/// - `c <= 0.04045`: `out = c / 12.92`
/// - `c >  0.04045`: `out = ((c + 0.055) / 1.055)^2.4`
#[inline]
pub fn srgb_to_linear(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that converting from Rec. 2020 to XYZ and back recovers the
    /// original value within the accumulated floating-point error of two
    /// sequential matrix multiplications.
    #[test]
    fn test_rec2020_xyz_roundtrip() {
        // Rec. 2020 unit white converted to XYZ and back must recover the
        // original triplet. Two sequential matrix multiplications with truncated
        // ITU coefficients accumulate up to approximately 1.2e-7 of
        // floating-point error.
        let white = (1.0_f64, 1.0, 1.0);
        let xyz = apply_matrix(white, &REC2020_TO_XYZ);
        let back = apply_matrix(xyz, &XYZ_TO_REC2020);
        assert!((back.0 - 1.0).abs() < 1e-6, "R: {}", back.0);
        assert!((back.1 - 1.0).abs() < 1e-6, "G: {}", back.1);
        assert!((back.2 - 1.0).abs() < 1e-6, "B: {}", back.2);
    }

    /// Verifies that converting an in-gamut sample from Rec. 709 to Rec. 2020 via CIE XYZ
    /// (D65), then back to Rec. 709, recovers the original *RGB* triplet within the
    /// accumulated floating-point error of four sequential matrix multiplications.
    #[test]
    fn test_rec709_rec2020_roundtrip() {
        // A colour converted from Rec. 709 to Rec. 2020 and back must
        // recover the original value within the accumulated error of four
        // sequential matrix multiplications.
        let colour = (0.8_f64, 0.4, 0.2);
        let rec2020 = rec709_to_rec2020(colour);
        let back = rec2020_to_rec709(rec2020);
        assert!((back.0 - colour.0).abs() < 1e-6);
        assert!((back.1 - colour.1).abs() < 1e-6);
        assert!((back.2 - colour.2).abs() < 1e-6);
    }

    /// Verifies that D65-aligned unit white `(1.0, 1.0, 1.0)` in linear Rec. 709 maps to
    /// unit white in linear Rec. 2020 within a tight tolerance, because both standards
    /// share the same D65 chromaticity anchor and the transform chain preserves neutrals.
    #[test]
    fn test_white_invariant_across_gamuts() {
        // D65 white (1.0, 1.0, 1.0) must remain white in any colour space,
        // since it is defined relative to the D65 white point in both Rec. 709
        // and Rec. 2020.
        let white = (1.0_f64, 1.0, 1.0);
        let result = rec709_to_rec2020(white);
        assert!((result.0 - 1.0).abs() < 1e-6);
        assert!((result.1 - 1.0).abs() < 1e-6);
        assert!((result.2 - 1.0).abs() < 1e-6);
    }

    /// Verifies that a fully saturated Rec. 2020 red `(1.0, 0.0, 0.0)` lies outside the
    /// Rec. 709 gamut triangle, by requiring at least one negative Rec. 709 channel after
    /// [`rec2020_to_rec709`], which is the expected signature of an out-of-gamut corner
    /// when projecting a wider basis into a narrower one without clipping.
    #[test]
    fn test_rec2020_wider_gamut_than_rec709() {
        // The red primary of Rec. 2020 lies outside the Rec. 709 gamut.
        // Converting a fully saturated Rec. 2020 red to Rec. 709 must
        // produce at least one negative channel value, confirming that
        // Rec. 2020 is indeed a wider colour space.
        let saturated_rec2020_red = (1.0_f64, 0.0, 0.0);
        let rec709 = rec2020_to_rec709(saturated_rec2020_red);
        assert!(
            rec709.1 < 0.0 || rec709.2 < 0.0,
            "Expected at least one negative Rec. 709 channel for a saturated Rec. 2020 \
             red primary, got: {rec709:?}"
        );
    }

    /// Verifies that applying the sRGB opto-electronic transfer function with
    /// [`linear_to_srgb`], then the inverse electro-optical function with [`srgb_to_linear`],
    /// recovers each representative linear code value within a tight tolerance, so the
    /// piecewise IEC 61966-2-1 pair behaves as a practical inverse on the sampled ladder.
    #[test]
    fn test_srgb_roundtrip() {
        let test_values = [0.0_f64, 0.01, 0.1, 0.5, 0.8, 1.0];
        for &v in &test_values {
            let encoded = linear_to_srgb(v);
            let decoded = srgb_to_linear(encoded);
            assert!(
                (decoded - v).abs() < 1e-10,
                "sRGB roundtrip failed at {v}: got {decoded}"
            );
        }
    }

    /// Verifies that [`linear_to_srgb`] maps linear black (`0.0`) and linear white (`1.0`)
    /// to the encoded extremes `0.0` and `1.0`, respectively, within tiny numerical slack,
    /// exercising the clamp and both segments of the forward transfer function at the
    /// boundaries of the nominal `[0.0, 1.0]` domain.
    #[test]
    fn test_srgb_black_and_white() {
        assert!((linear_to_srgb(0.0) - 0.0).abs() < 1e-12);
        assert!((linear_to_srgb(1.0) - 1.0).abs() < 1e-10);
    }
}
