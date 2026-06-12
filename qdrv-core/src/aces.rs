// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
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

use crate::{colors::apply_matrix, pq::pq_oetf_f64};

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

/// ACES AP0 (ACES2065-1) to ACES AP1 (ACEScg/rendering space).
///
/// Source: AMPAS ACES Core v1.3 CTL reference, `ACESlib.Transform_Common.ctl`,
/// where `AP0_2_AP1_MAT = AP0_2_XYZ_MAT * XYZ_2_AP1_MAT`; values below are
/// the same transform expressed for QDRV's column-vector matrix convention.
pub const ACES_AP0_TO_AP1: [[f64; 3]; 3] = [
    [1.4514393161, -0.2365107469, -0.2149285693],
    [-0.0765537734, 1.1762296998, -0.0996759264],
    [0.0083161484, -0.0060324498, 0.9977163014],
];

/// ACES AP1 (ACEScg/rendering space) to ACES AP0 (ACES2065-1).
///
/// Source: AMPAS ACES Core v1.3 CTL reference, `ACESlib.Transform_Common.ctl`,
/// where `AP1_2_AP0_MAT = AP1_2_XYZ_MAT * XYZ_2_AP0_MAT`; values below are
/// the same transform expressed for QDRV's column-vector matrix convention.
pub const ACES_AP1_TO_AP0: [[f64; 3]; 3] = [
    [0.6954522414, 0.1406786965, 0.1638690622],
    [0.0447945634, 0.8596711185, 0.0955343182],
    [-0.0055258826, 0.0040252103, 1.0015006723],
];

/// ACES AP1 (ACEScg/rendering space) to CIE XYZ using the ACES D60 white.
///
/// Source: AMPAS ACES Core v1.3 CTL reference, `ACESlib.Transform_Common.ctl`
/// and `ACESlib.Utilities_Color.ctl`, using the AP1 chromaticities.
pub const ACES_AP1_TO_XYZ: [[f64; 3]; 3] = [
    [0.6624541811, 0.1340042065, 0.1561876870],
    [0.2722287168, 0.6740817658, 0.0536895174],
    [-0.0055746495, 0.0040607335, 1.0103391003],
];

/// CIE XYZ using the ACES D60 white to ACES AP1 (ACEScg/rendering space).
///
/// Source: numerical inverse of [`ACES_AP1_TO_XYZ`], matching the AMPAS ACES
/// Core v1.3 CTL `XYZ_2_AP1_MAT` under QDRV's column-vector convention.
pub const XYZ_TO_ACES_AP1: [[f64; 3]; 3] = [
    [1.6410233797, -0.3248032942, -0.2364246952],
    [-0.6636628587, 1.6153315917, 0.0167563477],
    [0.0117218943, -0.0082844420, 0.9883948585],
];

/// Bradford chromatic-adaptation matrix from ACES white (D60) to D65.
///
/// Source: AMPAS ACES Core v1.3 CTL reference, `ACESlib.ODT_Common.ctl`
/// (`D60_2_D65_CAT = calculate_cat_matrix(AP0.white, REC709_PRI.white)`).
pub const ACES_D60_TO_D65_CAT: [[f64; 3]; 3] = [
    [0.9872240087, -0.0061132286, 0.0159532883],
    [-0.0075983718, 1.0018614847, 0.0053300358],
    [0.0030725771, -0.0050959615, 1.0816806031],
];

const REC709_XYZ_TO_RGB: [[f64; 3]; 3] = [
    [3.2409699, -1.5373832, -0.4986108],
    [-0.9692436, 1.8759675, 0.0415551],
    [0.0556301, -0.2039770, 1.0569715],
];

const REC2020_XYZ_TO_RGB: [[f64; 3]; 3] = [
    [1.7166511880, -0.3556707838, -0.2533662814],
    [-0.6666843518, 1.6164812366, 0.0157685458],
    [0.0176398574, -0.0427706133, 0.9421031212],
];

const REC2020_RGB_TO_XYZ: [[f64; 3]; 3] = [
    [0.6369580, 0.1446169, 0.1688810],
    [0.2627002, 0.6779981, 0.0593017],
    [0.0000000, 0.0280727, 1.0609851],
];

const AP1_RGB_TO_Y: [f64; 3] = [0.2722287168, 0.6740817658, 0.0536895174];
const HALF_MIN: f64 = 6.103515625e-5;
const HALF_MAX: f64 = 65_504.0;
const TINY: f64 = 1.0e-10;

#[derive(Debug, Clone, Copy)]
struct SplinePoint {
    x: f64,
    y: f64,
}

#[cfg(test)]
mod aces_output_tests {
    use super::*;

    #[test]
    fn aces_rrt_maps_neutral_middle_gray_to_oces_mid_point() {
        let oces = apply_rrt((0.18, 0.18, 0.18));
        assert!((oces.0 - 4.8).abs() < 1e-6, "OCES R: {}", oces.0);
        assert!((oces.1 - 4.8).abs() < 1e-6, "OCES G: {}", oces.1);
        assert!((oces.2 - 4.8).abs() < 1e-6, "OCES B: {}", oces.2);
    }

    #[test]
    fn aces_rec709_odt_keeps_neutral_output_bounded() {
        let rec709 = apply_odt_rec709_100nit(apply_rrt((0.18, 0.18, 0.18)));
        assert!((rec709.0 - rec709.1).abs() < 1e-6, "R/G: {rec709:?}");
        assert!((rec709.1 - rec709.2).abs() < 1e-6, "G/B: {rec709:?}");
        assert!(
            (0.35..0.45).contains(&rec709.0),
            "Rec.709 middle gray should be a bounded dim-surround code value, got {rec709:?}"
        );
    }

    #[test]
    fn aces_hdr_output_transforms_target_fifteen_nit_middle_gray() {
        let expected = crate::pq::nits_to_pq(15.0).expect("15 nits is inside ST 2084 range");
        let rec2020_1000 = apply_odt_rec2020_1000nit((0.18, 0.18, 0.18));
        let rec2020_4000 = apply_odt_rec2020_4000nit((0.18, 0.18, 0.18));

        for (label, rgb) in [("1000 nit", rec2020_1000), ("4000 nit", rec2020_4000)] {
            assert!((rgb.0 - expected).abs() < 1e-6, "{label} R: {}", rgb.0);
            assert!((rgb.1 - expected).abs() < 1e-6, "{label} G: {}", rgb.1);
            assert!((rgb.2 - expected).abs() < 1e-6, "{label} B: {}", rgb.2);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SegmentedSplineC5 {
    coefs_low: [f64; 6],
    coefs_high: [f64; 6],
    min_point: SplinePoint,
    mid_point: SplinePoint,
    max_point: SplinePoint,
    slope_low: f64,
    slope_high: f64,
}

#[derive(Debug, Clone, Copy)]
struct SegmentedSplineC9 {
    coefs_low: [f64; 10],
    coefs_high: [f64; 10],
    min_point: SplinePoint,
    mid_point: SplinePoint,
    max_point: SplinePoint,
    slope_low: f64,
    slope_high: f64,
}

#[derive(Debug, Clone, Copy)]
struct TsPoint {
    x: f64,
    y: f64,
    slope: f64,
}

#[derive(Debug, Clone, Copy)]
struct TsParams {
    min: TsPoint,
    mid: TsPoint,
    max: TsPoint,
    coefs_low: [f64; 6],
    coefs_high: [f64; 6],
}

const RRT_SPLINE: SegmentedSplineC5 = SegmentedSplineC5 {
    coefs_low: [
        -4.0000000000,
        -4.0000000000,
        -3.1573765773,
        -0.4852499958,
        1.8477324706,
        1.8477324706,
    ],
    coefs_high: [
        -0.7185482425,
        2.0810307172,
        3.6681241237,
        4.0000000000,
        4.0000000000,
        4.0000000000,
    ],
    min_point: SplinePoint {
        x: 0.18 / 32_768.0,
        y: 0.0001,
    },
    mid_point: SplinePoint { x: 0.18, y: 4.8 },
    max_point: SplinePoint {
        x: 0.18 * 262_144.0,
        y: 10_000.0,
    },
    slope_low: 0.0,
    slope_high: 0.0,
};

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

/// Applies the ACES 1.0 Reference Rendering Transform (RRT).
///
/// Input and output are linear ACES AP0 triplets. This is a Rust port of the
/// AMPAS ACES Core v1.3 CTL reference `RRT.ctl`: glow module, red modifier,
/// AP0->AP1 rendering-space conversion, RRT saturation adjustment, c5
/// segmented spline, and AP1->AP0 OCES output.
#[inline]
pub fn apply_rrt(aces_ap0: (f64, f64, f64)) -> (f64, f64, f64) {
    let rgb_pre = rrt_sweeteners(aces_ap0);
    let rgb_post = (
        segmented_spline_c5_fwd(rgb_pre.0, RRT_SPLINE),
        segmented_spline_c5_fwd(rgb_pre.1, RRT_SPLINE),
        segmented_spline_c5_fwd(rgb_pre.2, RRT_SPLINE),
    );
    apply_matrix(rgb_post, &ACES_AP1_TO_AP0)
}

/// Applies the ACES 1.0 Rec.709 100 nit dim-surround ODT.
///
/// Input is OCES from [`apply_rrt`]. Output is full-range Rec.709 code value
/// after BT.1886 inverse EOTF, matching the AMPAS ACES Core v1.3 CTL
/// `ODT.Academy.Rec709_100nits_dim.ctl` default `legalRange = 0` path.
#[inline]
pub fn apply_odt_rec709_100nit(oces_ap0: (f64, f64, f64)) -> (f64, f64, f64) {
    let rgb_pre = apply_matrix(oces_ap0, &ACES_AP0_TO_AP1);
    let odt = odt_48nits_spline();
    let rgb_post = (
        segmented_spline_c9_fwd(rgb_pre.0, odt),
        segmented_spline_c9_fwd(rgb_pre.1, odt),
        segmented_spline_c9_fwd(rgb_pre.2, odt),
    );

    let mut linear_cv = (
        y_to_linear_code_value(rgb_post.0, 48.0, cinema_black()),
        y_to_linear_code_value(rgb_post.1, 48.0, cinema_black()),
        y_to_linear_code_value(rgb_post.2, 48.0, cinema_black()),
    );
    linear_cv = dark_surround_to_dim_surround(linear_cv);
    linear_cv = apply_matrix(linear_cv, &saturation_matrix(0.93));

    let mut xyz = apply_matrix(linear_cv, &ACES_AP1_TO_XYZ);
    xyz = apply_matrix(xyz, &ACES_D60_TO_D65_CAT);
    linear_cv = apply_matrix(xyz, &REC709_XYZ_TO_RGB);
    linear_cv = clamp_rgb(linear_cv, 0.0, 1.0);

    (
        bt1886_r(linear_cv.0, 2.4, 1.0, 0.0),
        bt1886_r(linear_cv.1, 2.4, 1.0, 0.0),
        bt1886_r(linear_cv.2, 2.4, 1.0, 0.0),
    )
}

/// Applies the ACES 1.0 Rec.2020 ST2084 1000 nit output transform.
///
/// The ACES v1.3 CTL reference publishes the HDR Rec.2020 ST2084 targets as
/// combined RRT+ODT transforms (`RRTODT.Academy.Rec2020_1000nits_15nits_ST2084.ctl`).
/// For that reason this function accepts ACES AP0 input directly and returns
/// full-range Rec.2020 PQ code values after the complete published transform.
#[inline]
pub fn apply_odt_rec2020_1000nit(aces_ap0: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_rec2020_st2084_output_transform(aces_ap0, 1_000.0)
}

/// Applies the ACES 1.0 Rec.2020 ST2084 4000 nit output transform.
///
/// See [`apply_odt_rec2020_1000nit`] for the note on the ACES v1.3 HDR
/// output transforms being published as combined RRT+ODT CTL programs.
#[inline]
pub fn apply_odt_rec2020_4000nit(aces_ap0: (f64, f64, f64)) -> (f64, f64, f64) {
    apply_rec2020_st2084_output_transform(aces_ap0, 4_000.0)
}

fn apply_rec2020_st2084_output_transform(
    aces_ap0: (f64, f64, f64),
    peak_nits: f64,
) -> (f64, f64, f64) {
    let default_params = init_ts_params(0.0001, peak_nits, 0.0);
    let exp_shift = inv_ssts(15.0, default_params).log2() - 0.18_f64.log2();
    let params = init_ts_params(0.0001, peak_nits, exp_shift);

    let rgb_pre = rrt_sweeteners(aces_ap0);
    let rgb_post = (
        ssts(rgb_pre.0, params),
        ssts(rgb_pre.1, params),
        ssts(rgb_pre.2, params),
    );
    let linear_cv = (
        y_to_linear_code_value(rgb_post.0, peak_nits, 0.0001),
        y_to_linear_code_value(rgb_post.1, peak_nits, 0.0001),
        y_to_linear_code_value(rgb_post.2, peak_nits, 0.0001),
    );

    let mut xyz = apply_matrix(linear_cv, &ACES_AP1_TO_XYZ);
    xyz = limit_to_rec2020_primaries(xyz);
    xyz = apply_matrix(xyz, &ACES_D60_TO_D65_CAT);
    let linear_cv = clamp_rgb_min(apply_matrix(xyz, &REC2020_XYZ_TO_RGB), 0.0);

    (
        pq_oetf_f64((linear_code_value_to_y(linear_cv.0, peak_nits, 0.0) / 10_000.0).max(0.0)),
        pq_oetf_f64((linear_code_value_to_y(linear_cv.1, peak_nits, 0.0) / 10_000.0).max(0.0)),
        pq_oetf_f64((linear_code_value_to_y(linear_cv.2, peak_nits, 0.0) / 10_000.0).max(0.0)),
    )
}

fn rrt_sweeteners(aces_ap0: (f64, f64, f64)) -> (f64, f64, f64) {
    let saturation = rgb_saturation(aces_ap0);
    let yc = rgb_yc(aces_ap0);
    let shaped_saturation = sigmoid_shaper((saturation - 0.4) / 0.2);
    let added_glow = 1.0 + glow_fwd(yc, 0.05 * shaped_saturation, 0.08);
    let mut aces = scale_rgb(aces_ap0, added_glow);

    let hue = rgb_hue(aces);
    let centered_hue = center_hue(hue, 0.0);
    let hue_weight = cubic_basis_shaper(centered_hue, 135.0);
    aces.0 += hue_weight * saturation * (0.03 - aces.0) * (1.0 - 0.82);

    aces = clamp_rgb_min(aces, 0.0);
    let mut rgb_pre = apply_matrix(aces, &ACES_AP0_TO_AP1);
    rgb_pre = clamp_rgb(rgb_pre, 0.0, HALF_MAX);
    apply_matrix(rgb_pre, &saturation_matrix(0.96))
}

fn rgb_saturation(rgb: (f64, f64, f64)) -> f64 {
    (max3(rgb).max(TINY) - min3(rgb).max(TINY)) / max3(rgb).max(1.0e-2)
}

fn rgb_yc(rgb: (f64, f64, f64)) -> f64 {
    let chroma = (rgb.2 * (rgb.2 - rgb.1) + rgb.1 * (rgb.1 - rgb.0) + rgb.0 * (rgb.0 - rgb.2))
        .max(0.0)
        .sqrt();
    (rgb.2 + rgb.1 + rgb.0 + 1.75 * chroma) / 3.0
}

fn rgb_hue(rgb: (f64, f64, f64)) -> f64 {
    if rgb.0 == rgb.1 && rgb.1 == rgb.2 {
        return f64::NAN;
    }
    let mut hue = (3.0_f64.sqrt() * (rgb.1 - rgb.2))
        .atan2(2.0 * rgb.0 - rgb.1 - rgb.2)
        .to_degrees();
    if hue < 0.0 {
        hue += 360.0;
    }
    hue
}

fn glow_fwd(yc: f64, glow_gain: f64, glow_mid: f64) -> f64 {
    if yc <= (2.0 / 3.0) * glow_mid {
        glow_gain
    } else if yc >= 2.0 * glow_mid {
        0.0
    } else {
        glow_gain * (glow_mid / yc - 0.5)
    }
}

fn sigmoid_shaper(x: f64) -> f64 {
    let t = (1.0 - (x / 2.0).abs()).max(0.0);
    (1.0 + signum_ctl(x) * (1.0 - t * t)) / 2.0
}

fn center_hue(hue: f64, center: f64) -> f64 {
    let mut centered = hue - center;
    if centered < -180.0 {
        centered += 360.0;
    } else if centered > 180.0 {
        centered -= 360.0;
    }
    centered
}

fn cubic_basis_shaper(x: f64, width: f64) -> f64 {
    let knots = [-width / 2.0, -width / 4.0, 0.0, width / 4.0, width / 2.0];
    if !(x > knots[0] && x < knots[4]) {
        return 0.0;
    }

    let knot_coord = (x - knots[0]) * 4.0 / width;
    let segment = knot_coord as i32;
    let t = knot_coord - f64::from(segment);
    let monomials = [t * t * t, t * t, t, 1.0];
    let value = match segment {
        3 => {
            monomials[0] * (-1.0 / 6.0)
                + monomials[1] * (3.0 / 6.0)
                + monomials[2] * (-3.0 / 6.0)
                + monomials[3] * (1.0 / 6.0)
        }
        2 => monomials[0] * (3.0 / 6.0) + monomials[1] * (-6.0 / 6.0) + monomials[3] * (4.0 / 6.0),
        1 => {
            monomials[0] * (-3.0 / 6.0)
                + monomials[1] * (3.0 / 6.0)
                + monomials[2] * (3.0 / 6.0)
                + monomials[3] * (1.0 / 6.0)
        }
        0 => monomials[0] * (1.0 / 6.0),
        _ => 0.0,
    };
    value * 1.5
}

fn segmented_spline_c5_fwd(x: f64, params: SegmentedSplineC5) -> f64 {
    let log_x = x.max(HALF_MIN).log10();
    let log_y = if log_x <= params.min_point.x.log10() {
        log_x * params.slope_low
            + (params.min_point.y.log10() - params.slope_low * params.min_point.x.log10())
    } else if log_x < params.mid_point.x.log10() {
        let knot_coord = 3.0 * (log_x - params.min_point.x.log10())
            / (params.mid_point.x.log10() - params.min_point.x.log10());
        let segment = (knot_coord.floor() as usize).min(3);
        let t = knot_coord - segment as f64;
        spline_quadratic(
            [
                params.coefs_low[segment],
                params.coefs_low[segment + 1],
                params.coefs_low[segment + 2],
            ],
            t,
        )
    } else if log_x < params.max_point.x.log10() {
        let knot_coord = 3.0 * (log_x - params.mid_point.x.log10())
            / (params.max_point.x.log10() - params.mid_point.x.log10());
        let segment = (knot_coord.floor() as usize).min(3);
        let t = knot_coord - segment as f64;
        spline_quadratic(
            [
                params.coefs_high[segment],
                params.coefs_high[segment + 1],
                params.coefs_high[segment + 2],
            ],
            t,
        )
    } else {
        log_x * params.slope_high
            + (params.max_point.y.log10() - params.slope_high * params.max_point.x.log10())
    };
    10.0_f64.powf(log_y)
}

fn segmented_spline_c9_fwd(x: f64, params: SegmentedSplineC9) -> f64 {
    let log_x = x.max(HALF_MIN).log10();
    let log_y = if log_x <= params.min_point.x.log10() {
        log_x * params.slope_low
            + (params.min_point.y.log10() - params.slope_low * params.min_point.x.log10())
    } else if log_x < params.mid_point.x.log10() {
        let knot_coord = 7.0 * (log_x - params.min_point.x.log10())
            / (params.mid_point.x.log10() - params.min_point.x.log10());
        let segment = (knot_coord.floor() as usize).min(7);
        let t = knot_coord - segment as f64;
        spline_quadratic(
            [
                params.coefs_low[segment],
                params.coefs_low[segment + 1],
                params.coefs_low[segment + 2],
            ],
            t,
        )
    } else if log_x < params.max_point.x.log10() {
        let knot_coord = 7.0 * (log_x - params.mid_point.x.log10())
            / (params.max_point.x.log10() - params.mid_point.x.log10());
        let segment = (knot_coord.floor() as usize).min(7);
        let t = knot_coord - segment as f64;
        spline_quadratic(
            [
                params.coefs_high[segment],
                params.coefs_high[segment + 1],
                params.coefs_high[segment + 2],
            ],
            t,
        )
    } else {
        log_x * params.slope_high
            + (params.max_point.y.log10() - params.slope_high * params.max_point.x.log10())
    };
    10.0_f64.powf(log_y)
}

fn spline_quadratic(coefs: [f64; 3], t: f64) -> f64 {
    let basis_0 = 0.5 * coefs[0] - coefs[1] + 0.5 * coefs[2];
    let basis_1 = -coefs[0] + coefs[1];
    let basis_2 = 0.5 * coefs[0] + 0.5 * coefs[1];
    t * t * basis_0 + t * basis_1 + basis_2
}

fn odt_48nits_spline() -> SegmentedSplineC9 {
    SegmentedSplineC9 {
        coefs_low: [
            -1.6989700043,
            -1.6989700043,
            -1.4779000000,
            -1.2291000000,
            -0.8648000000,
            -0.4480000000,
            0.0051800000,
            0.4511080334,
            0.9113744414,
            0.9113744414,
        ],
        coefs_high: [
            0.5154386965,
            0.8470437783,
            1.1358000000,
            1.3802000000,
            1.5197000000,
            1.5985000000,
            1.6467000000,
            1.6746091357,
            1.6878733390,
            1.6878733390,
        ],
        min_point: SplinePoint {
            x: segmented_spline_c5_fwd(0.18 * 2.0_f64.powf(-6.5), RRT_SPLINE),
            y: 0.02,
        },
        mid_point: SplinePoint {
            x: segmented_spline_c5_fwd(0.18, RRT_SPLINE),
            y: 4.8,
        },
        max_point: SplinePoint {
            x: segmented_spline_c5_fwd(0.18 * 2.0_f64.powf(6.5), RRT_SPLINE),
            y: 48.0,
        },
        slope_low: 0.0,
        slope_high: 0.04,
    }
}

fn init_ts_params(min_lum: f64, max_lum: f64, exp_shift: f64) -> TsParams {
    let base_min = lookup_aces_min(min_lum);
    let base_max = lookup_aces_max(max_lum);
    let mut min = TsPoint {
        x: base_min,
        y: min_lum,
        slope: 0.0,
    };
    let mut mid = TsPoint {
        x: 0.18,
        y: 4.8,
        slope: 1.55,
    };
    let mut max = TsPoint {
        x: base_max,
        y: max_lum,
        slope: 0.0,
    };
    let coefs_low_src = init_coefs_low(min, mid);
    let coefs_high_src = init_coefs_high(mid, max);

    min.x = shift(base_min, exp_shift);
    mid.x = shift(0.18, exp_shift);
    max.x = shift(base_max, exp_shift);
    TsParams {
        min,
        mid,
        max,
        coefs_low: [
            coefs_low_src[0],
            coefs_low_src[1],
            coefs_low_src[2],
            coefs_low_src[3],
            coefs_low_src[4],
            coefs_low_src[4],
        ],
        coefs_high: [
            coefs_high_src[0],
            coefs_high_src[1],
            coefs_high_src[2],
            coefs_high_src[3],
            coefs_high_src[4],
            coefs_high_src[4],
        ],
    }
}

fn ssts(x: f64, params: TsParams) -> f64 {
    let log_x = x.max(HALF_MIN).log10();
    let log_y = if log_x <= params.min.x.log10() {
        log_x * params.min.slope + (params.min.y.log10() - params.min.slope * params.min.x.log10())
    } else if log_x < params.mid.x.log10() {
        let knot_coord =
            3.0 * (log_x - params.min.x.log10()) / (params.mid.x.log10() - params.min.x.log10());
        let segment = (knot_coord.floor() as usize).min(3);
        let t = knot_coord - segment as f64;
        spline_quadratic(
            [
                params.coefs_low[segment],
                params.coefs_low[segment + 1],
                params.coefs_low[segment + 2],
            ],
            t,
        )
    } else if log_x < params.max.x.log10() {
        let knot_coord =
            3.0 * (log_x - params.mid.x.log10()) / (params.max.x.log10() - params.mid.x.log10());
        let segment = (knot_coord.floor() as usize).min(3);
        let t = knot_coord - segment as f64;
        spline_quadratic(
            [
                params.coefs_high[segment],
                params.coefs_high[segment + 1],
                params.coefs_high[segment + 2],
            ],
            t,
        )
    } else {
        log_x * params.max.slope + (params.max.y.log10() - params.max.slope * params.max.x.log10())
    };
    10.0_f64.powf(log_y)
}

fn inv_ssts(y: f64, params: TsParams) -> f64 {
    let knot_inc_low = (params.mid.x.log10() - params.min.x.log10()) / 3.0;
    let knot_inc_high = (params.max.x.log10() - params.mid.x.log10()) / 3.0;
    let knot_y_low = [
        (params.coefs_low[0] + params.coefs_low[1]) / 2.0,
        (params.coefs_low[1] + params.coefs_low[2]) / 2.0,
        (params.coefs_low[2] + params.coefs_low[3]) / 2.0,
        (params.coefs_low[3] + params.coefs_low[4]) / 2.0,
    ];
    let knot_y_high = [
        (params.coefs_high[0] + params.coefs_high[1]) / 2.0,
        (params.coefs_high[1] + params.coefs_high[2]) / 2.0,
        (params.coefs_high[2] + params.coefs_high[3]) / 2.0,
        (params.coefs_high[3] + params.coefs_high[4]) / 2.0,
    ];
    let log_y = y.max(1.0e-10).log10();
    let log_x = if log_y <= params.min.y.log10() {
        params.min.x.log10()
    } else if log_y <= params.mid.y.log10() {
        let segment = if log_y > knot_y_low[0] && log_y <= knot_y_low[1] {
            0
        } else if log_y > knot_y_low[1] && log_y <= knot_y_low[2] {
            1
        } else {
            2
        };
        inverse_spline_log_x(
            [
                params.coefs_low[segment],
                params.coefs_low[segment + 1],
                params.coefs_low[segment + 2],
            ],
            log_y,
            params.min.x.log10(),
            knot_inc_low,
            segment,
        )
    } else if log_y < params.max.y.log10() {
        let segment = if log_y >= knot_y_high[0] && log_y <= knot_y_high[1] {
            0
        } else if log_y > knot_y_high[1] && log_y <= knot_y_high[2] {
            1
        } else {
            2
        };
        inverse_spline_log_x(
            [
                params.coefs_high[segment],
                params.coefs_high[segment + 1],
                params.coefs_high[segment + 2],
            ],
            log_y,
            params.mid.x.log10(),
            knot_inc_high,
            segment,
        )
    } else {
        params.max.x.log10()
    };
    10.0_f64.powf(log_x)
}

fn inverse_spline_log_x(
    coefs: [f64; 3],
    log_y: f64,
    base_log_x: f64,
    knot_inc: f64,
    segment: usize,
) -> f64 {
    let basis_0 = 0.5 * coefs[0] - coefs[1] + 0.5 * coefs[2];
    let basis_1 = -coefs[0] + coefs[1];
    let basis_2 = 0.5 * coefs[0] + 0.5 * coefs[1] - log_y;
    let discriminant = (basis_1 * basis_1 - 4.0 * basis_0 * basis_2)
        .max(0.0)
        .sqrt();
    let t = (2.0 * basis_2) / (-discriminant - basis_1);
    base_log_x + (t + segment as f64) * knot_inc
}

fn init_coefs_low(low: TsPoint, mid: TsPoint) -> [f64; 5] {
    let knot_inc = (mid.x.log10() - low.x.log10()) / 3.0;
    let mut coefs = [0.0; 5];
    coefs[0] =
        low.slope * (low.x.log10() - 0.5 * knot_inc) + (low.y.log10() - low.slope * low.x.log10());
    coefs[1] =
        low.slope * (low.x.log10() + 0.5 * knot_inc) + (low.y.log10() - low.slope * low.x.log10());
    coefs[3] =
        mid.slope * (mid.x.log10() - 0.5 * knot_inc) + (mid.y.log10() - mid.slope * mid.x.log10());
    coefs[4] =
        mid.slope * (mid.x.log10() + 0.5 * knot_inc) + (mid.y.log10() - mid.slope * mid.x.log10());
    let pct_low = interpolate_1d([(-15.0, 0.18), (-6.5, 0.35)], (low.x / 0.18).log2());
    coefs[2] = low.y.log10() + pct_low * (mid.y.log10() - low.y.log10());
    coefs
}

fn init_coefs_high(mid: TsPoint, max: TsPoint) -> [f64; 5] {
    let knot_inc = (max.x.log10() - mid.x.log10()) / 3.0;
    let mut coefs = [0.0; 5];
    coefs[0] =
        mid.slope * (mid.x.log10() - 0.5 * knot_inc) + (mid.y.log10() - mid.slope * mid.x.log10());
    coefs[1] =
        mid.slope * (mid.x.log10() + 0.5 * knot_inc) + (mid.y.log10() - mid.slope * mid.x.log10());
    coefs[3] =
        max.slope * (max.x.log10() - 0.5 * knot_inc) + (max.y.log10() - max.slope * max.x.log10());
    coefs[4] =
        max.slope * (max.x.log10() + 0.5 * knot_inc) + (max.y.log10() - max.slope * max.x.log10());
    let pct_high = interpolate_1d([(6.5, 0.89), (18.0, 0.90)], (max.x / 0.18).log2());
    coefs[2] = mid.y.log10() + pct_high * (max.y.log10() - mid.y.log10());
    coefs
}

fn lookup_aces_min(min_lum: f64) -> f64 {
    0.18 * 2.0_f64.powf(interpolate_1d(
        [(0.0001_f64.log10(), -15.0), (0.02_f64.log10(), -6.5)],
        min_lum.log10(),
    ))
}

fn lookup_aces_max(max_lum: f64) -> f64 {
    0.18 * 2.0_f64.powf(interpolate_1d(
        [(48.0_f64.log10(), 6.5), (10_000.0_f64.log10(), 18.0)],
        max_lum.log10(),
    ))
}

fn interpolate_1d(points: [(f64, f64); 2], x: f64) -> f64 {
    let (x0, y0) = points[0];
    let (x1, y1) = points[1];
    y0 + (x - x0) * (y1 - y0) / (x1 - x0)
}

fn shift(value: f64, exp_shift: f64) -> f64 {
    2.0_f64.powf(value.log2() - exp_shift)
}

fn y_to_linear_code_value(y: f64, y_max: f64, y_min: f64) -> f64 {
    (y - y_min) / (y_max - y_min)
}

fn linear_code_value_to_y(linear_cv: f64, y_max: f64, y_min: f64) -> f64 {
    linear_cv * (y_max - y_min) + y_min
}

fn dark_surround_to_dim_surround(linear_cv: (f64, f64, f64)) -> (f64, f64, f64) {
    let xyz = apply_matrix(linear_cv, &ACES_AP1_TO_XYZ);
    let mut xyy = xyz_to_xyy(xyz);
    xyy.2 = xyy.2.clamp(0.0, f64::INFINITY).powf(0.9811);
    apply_matrix(xyy_to_xyz(xyy), &XYZ_TO_ACES_AP1)
}

fn limit_to_rec2020_primaries(xyz: (f64, f64, f64)) -> (f64, f64, f64) {
    let limited_rgb = clamp_rgb(apply_matrix(xyz, &REC2020_XYZ_TO_RGB), 0.0, 1.0);
    apply_matrix(limited_rgb, &REC2020_RGB_TO_XYZ)
}

fn xyz_to_xyy(xyz: (f64, f64, f64)) -> (f64, f64, f64) {
    let divisor = (xyz.0 + xyz.1 + xyz.2).max(1.0e-10);
    (xyz.0 / divisor, xyz.1 / divisor, xyz.1)
}

fn xyy_to_xyz(xyy: (f64, f64, f64)) -> (f64, f64, f64) {
    let y = xyy.1.max(1.0e-10);
    (xyy.0 * xyy.2 / y, xyy.2, (1.0 - xyy.0 - xyy.1) * xyy.2 / y)
}

fn bt1886_r(luminance: f64, gamma: f64, white: f64, black: f64) -> f64 {
    let white_root = white.powf(1.0 / gamma);
    let black_root = black.powf(1.0 / gamma);
    let a = (white_root - black_root).powf(gamma);
    let b = black_root / (white_root - black_root);
    (luminance / a).max(0.0).powf(1.0 / gamma) - b
}

fn saturation_matrix(saturation: f64) -> [[f64; 3]; 3] {
    let inverse = 1.0 - saturation;
    [
        [
            inverse * AP1_RGB_TO_Y[0] + saturation,
            inverse * AP1_RGB_TO_Y[1],
            inverse * AP1_RGB_TO_Y[2],
        ],
        [
            inverse * AP1_RGB_TO_Y[0],
            inverse * AP1_RGB_TO_Y[1] + saturation,
            inverse * AP1_RGB_TO_Y[2],
        ],
        [
            inverse * AP1_RGB_TO_Y[0],
            inverse * AP1_RGB_TO_Y[1],
            inverse * AP1_RGB_TO_Y[2] + saturation,
        ],
    ]
}

fn scale_rgb(rgb: (f64, f64, f64), scale: f64) -> (f64, f64, f64) {
    (rgb.0 * scale, rgb.1 * scale, rgb.2 * scale)
}

fn clamp_rgb(rgb: (f64, f64, f64), min: f64, max: f64) -> (f64, f64, f64) {
    (
        rgb.0.clamp(min, max),
        rgb.1.clamp(min, max),
        rgb.2.clamp(min, max),
    )
}

fn clamp_rgb_min(rgb: (f64, f64, f64), min: f64) -> (f64, f64, f64) {
    (rgb.0.max(min), rgb.1.max(min), rgb.2.max(min))
}

fn min3(rgb: (f64, f64, f64)) -> f64 {
    rgb.0.min(rgb.1).min(rgb.2)
}

fn max3(rgb: (f64, f64, f64)) -> f64 {
    rgb.0.max(rgb.1).max(rgb.2)
}

fn signum_ctl(x: f64) -> f64 {
    if x < 0.0 {
        -1.0
    } else if x > 0.0 {
        1.0
    } else {
        0.0
    }
}

fn cinema_black() -> f64 {
    10.0_f64.powf(0.02_f64.log10())
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
