// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! SDR fallback generation for QDRV delivery-tier streams.
//!
//! Tone-maps and gamut-maps QDRV delivery-tier pixels to 8-bit sRGB output
//! suitable for legacy displays (Rec. 709 primaries, sRGB transfer function,
//! ~100 cd/m² reference white).
//!
//! ## Design
//!
//! * **Luminance-preserving tone mapping.** The per-frame tone curve is
//!   evaluated on a single luminance scalar per pixel (Rec. 2100 NCL luma),
//!   and the resulting scalar is applied uniformly to R, G, and B. Per-channel
//!   tone mapping shifts hue and desaturates highlights; the luminance-based
//!   formulation keeps saturated colours saturated.
//!
//! * **Curve domain.** The tone curve is evaluated in the reference-display
//!   normalised linear-light domain (1.0 ≡ reference display peak), matching
//!   the convention used by [`crate::tone_map`]. The curve output is the
//!   fraction of the *target* display's peak luminance; for SDR the target
//!   peak is [`SDR_WHITE_NITS`] and the output therefore lives in the
//!   display-referred linear range `[0.0, 1.0]` where `1.0 ≡ SDR white`.
//!
//! * **Soft gamut mapping.** After the linear Rec. 2020 → linear Rec. 709
//!   conversion via CIE XYZ (D65 is shared), pixels outside the Rec. 709
//!   triangle are desaturated toward the achromatic axis at their own
//!   luminance instead of being hard-clamped per channel. Hard per-channel
//!   clamping posterises out-of-gamut highlights; the soft method rolls them
//!   off gracefully.
//!
//! * **Triangular-PDF dither.** The final 8-bit quantisation adds a
//!   ±½-LSB TPDF noise sample per channel, seeded deterministically from the
//!   pixel index so that identical input frames always produce identical
//!   output frames. This removes the visible banding that bare
//!   `(x * 255.0).round()` quantisation produces on smooth HDR gradients.
//!
//! ## Pipeline (per pixel)
//!
//! 1. Decode the ST 2084 PQ signal per channel to linear normalised light.
//! 2. Compute luminance with the Rec. 2100 non-constant-luminance coefficients.
//! 3. Normalise luminance to the reference display peak and evaluate the
//!    per-frame tone curve.
//! 4. Apply the scene-to-display scalar uniformly to R, G, and B.
//! 5. Convert from linear Rec. 2020 to linear Rec. 709 via CIE XYZ (D65).
//! 6. Desaturate out-of-gamut channels toward luminance (soft gamut clip).
//! 7. Apply the sRGB OETF (IEC 61966-2-1).
//! 8. TPDF-dither and quantise to 8-bit `[0, 255]`.

use qdrv_core::{
    colors::{
        REC2020_TO_XYZ, XYZ_TO_REC709, apply_matrix, linear_to_srgb,
        ncl::{KB as NCL_KB, KG as NCL_KG, KR as NCL_KR},
    },
    pixel::Pixel32,
    pq::{PQ_MAX_NITS, pq_eotf_f32},
};
use qdrv_meta::{DynamicMeta, ToneMapCurve};

/// SDR reference white luminance in cd/m². A linear output of `1.0` from this
/// module corresponds to this luminance on a canonical SDR display.
pub const SDR_WHITE_NITS: f32 = 100.0;

// ITU-R Rec. 2100 non-constant luminance coefficients, duplicated locally as
// `f32` for the hot per-pixel loop. The Float64 source of truth lives in
// [`qdrv_core::colors::ncl`] — these constants are derived from those via
// `as f32` casts so the values cannot drift.
const KR: f32 = NCL_KR as f32;
const KG: f32 = NCL_KG as f32;
const KB: f32 = NCL_KB as f32;

/// Tone-maps and gamut-maps a buffer of QDRV delivery-tier pixels to 8-bit
/// sRGB for SDR display.
///
/// Returns one `[R, G, B]` triplet per input pixel, each channel in
/// `[0, 255]` after TPDF dither.
pub fn tone_map_to_sdr(pixels: &[Pixel32], dynamic: &DynamicMeta) -> Vec<[u8; 3]> {
    // The reference display peak may legitimately sit below the SDR white
    // point for SDR-authored content; clamp at SDR_WHITE_NITS to avoid a
    // division that would inflate all luminance values and saturate the
    // output uniformly.
    let ref_max = dynamic
        .target_display_hint
        .max_luminance_nits
        .max(SDR_WHITE_NITS);

    // Pre-multiply `PQ_MAX_NITS / ref_max` so the per-pixel inner loop only
    // needs a single multiplication to convert the PQ-normalised luminance
    // (1.0 ≡ 10 000 nits) into the reference-display fraction that the tone
    // curve expects (1.0 ≡ ref_max).
    let linear_to_ref_fraction = (PQ_MAX_NITS as f32) / ref_max;
    let curve = &dynamic.tone_map_curve;

    pixels
        .iter()
        .enumerate()
        .map(|(idx, p)| tone_map_pixel_to_sdr(p, curve, linear_to_ref_fraction, idx as u64))
        .collect()
}

/// Applies the full SDR pipeline to a single pixel.
#[inline]
fn tone_map_pixel_to_sdr(
    p: &Pixel32,
    curve: &ToneMapCurve,
    linear_to_ref_fraction: f32,
    pixel_idx: u64,
) -> [u8; 3] {
    // 1. PQ → linear normalised (1.0 ≡ PQ_MAX_NITS nits).
    let r_lin = pq_eotf_f32(p.r);
    let g_lin = pq_eotf_f32(p.g);
    let b_lin = pq_eotf_f32(p.b);

    // 2. Scene luminance (Rec. 2100 NCL), same normalisation as the channels.
    let y_lin = KR * r_lin + KG * g_lin + KB * b_lin;

    // 3. Evaluate the tone curve in the reference-display-normalised linear
    //    domain. The curve is defined on `[0.0, 1.0]`; highlights brighter
    //    than the reference display are clamped to 1.0 before evaluation,
    //    which matches `tone_map.rs` and relies on the curve's top anchor
    //    (`input = 1.0`) to provide a valid output for saturated input.
    let y_ref = (y_lin * linear_to_ref_fraction).clamp(0.0, 1.0);
    let mapped = curve.evaluate(y_ref);

    // 4. Luminance-preserving chromaticity scale.
    //
    //    The tone curve output `mapped` is in display-referred linear light
    //    where `1.0 ≡ SDR_WHITE_NITS` (target display peak for the SDR
    //    pipeline). `y_lin` is in scene-referred linear light where
    //    `1.0 ≡ PQ_MAX_NITS`. Applying the single scalar `mapped / y_lin`
    //    uniformly to R, G, B converts the whole pixel from scene-referred
    //    to display-referred space while preserving chromaticity:
    //
    //        R_nits_out = r_lin * PQ_MAX_NITS * (mapped * SDR_WHITE_NITS) /
    //                                           (y_lin * PQ_MAX_NITS)
    //        R_sdr      = R_nits_out / SDR_WHITE_NITS = r_lin * mapped / y_lin
    //
    //    The `PQ_MAX_NITS` and `SDR_WHITE_NITS` factors cancel out when the
    //    output is expressed in SDR-display-referred units.
    let chroma_scale = if y_lin > 1.0e-7 { mapped / y_lin } else { 0.0 };

    // Promote to Float64 for the gamut conversion and sRGB OETF; the matrices
    // and transfer function in `qdrv-core` are Float64 throughout to meet the
    // mastering precision target. The conversion cost is negligible.
    let r_2020 = (r_lin * chroma_scale) as f64;
    let g_2020 = (g_lin * chroma_scale) as f64;
    let b_2020 = (b_lin * chroma_scale) as f64;

    // 5. Linear Rec. 2020 → linear Rec. 709 via CIE XYZ (D65 shared).
    let xyz = apply_matrix((r_2020, g_2020, b_2020), &REC2020_TO_XYZ);
    let (r_709, g_709, b_709) = apply_matrix(xyz, &XYZ_TO_REC709);

    // 6. Soft gamut mapping: desaturate out-of-gamut samples toward the
    //    achromatic axis at their own Rec. 709 luminance. Hard per-channel
    //    clamping would posterise saturated HDR primaries; this method
    //    preserves hue and rolls off saturation smoothly.
    let y_709 = KR as f64 * r_709 + KG as f64 * g_709 + KB as f64 * b_709;
    let (r_ing, g_ing, b_ing) = desaturate_to_gamut((r_709, g_709, b_709), y_709);

    // 7. sRGB OETF — inputs are now inside `[0.0, 1.0]` per channel.
    let r_srgb = linear_to_srgb(r_ing);
    let g_srgb = linear_to_srgb(g_ing);
    let b_srgb = linear_to_srgb(b_ing);

    // 8. TPDF dither + 8-bit quantise. The RNG is seeded deterministically
    //    from `pixel_idx`, so output is bit-exact across repeated invocations.
    let mut rng = splitmix64_seed(pixel_idx);
    let tr = tpdf_sample(&mut rng);
    let tg = tpdf_sample(&mut rng);
    let tb = tpdf_sample(&mut rng);

    [
        quantise_u8_tpdf(r_srgb, tr),
        quantise_u8_tpdf(g_srgb, tg),
        quantise_u8_tpdf(b_srgb, tb),
    ]
}

/// Desaturates an out-of-gamut `(r, g, b)` triplet toward the achromatic
/// axis at its own luminance `y`, until every channel lies in `[0.0, 1.0]`.
/// In-gamut triplets are returned unchanged.
///
/// This is equivalent to a linear interpolation between the input colour and
/// its luminance-matched neutral, with the blend factor `t` chosen as the
/// smallest value that brings the worst offending channel back into the
/// `[0.0, 1.0]` unit cube. The result preserves luminance exactly (because
/// the neutral has the same luminance as the input) and rolls off saturation
/// smoothly instead of posterising at the gamut boundary.
#[inline]
fn desaturate_to_gamut(rgb: (f64, f64, f64), y: f64) -> (f64, f64, f64) {
    let (r, g, b) = rgb;
    let min_ch = r.min(g).min(b);
    let max_ch = r.max(g).max(b);

    let mut t = 0.0_f64;

    // Required blend to lift the minimum channel to 0.
    if min_ch < 0.0 {
        let denom = y - min_ch;
        if denom > 1.0e-12 {
            t = t.max(-min_ch / denom);
        } else {
            t = 1.0;
        }
    }
    // Required blend to bring the maximum channel down to 1.
    if max_ch > 1.0 {
        let denom = max_ch - y;
        if denom > 1.0e-12 {
            t = t.max((max_ch - 1.0) / denom);
        } else {
            t = 1.0;
        }
    }

    if t <= 0.0 {
        return rgb;
    }
    let t = t.min(1.0);
    let k = 1.0 - t;

    // A final per-channel clamp absorbs any rounding residue so the output is
    // guaranteed to lie in `[0.0, 1.0]` before the sRGB OETF.
    (
        (y + k * (r - y)).clamp(0.0, 1.0),
        (y + k * (g - y)).clamp(0.0, 1.0),
        (y + k * (b - y)).clamp(0.0, 1.0),
    )
}

/// Quantises a linear-sRGB-encoded channel value in `[0.0, 1.0]` to 8-bit
/// with a ±½-LSB TPDF dither sample applied before rounding.
#[inline]
fn quantise_u8_tpdf(v: f64, tpdf: f64) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let q = (v * 255.0 + tpdf * 0.5).round();
    q.clamp(0.0, 255.0) as u8
}

/// Draws a TPDF sample in `[-1.0, 1.0)` as the difference of two independent
/// uniform samples in `[0.0, 1.0)`. TPDF noise gives flat spectral density
/// near DC and decorrelates quantisation error from the signal, which is
/// what we want for smooth gradient quantisation.
#[inline]
fn tpdf_sample(rng: &mut u64) -> f64 {
    uniform_01(rng) - uniform_01(rng)
}

/// SplitMix64 step, producing a uniform sample in `[0.0, 1.0)` with 53 bits
/// of mantissa entropy. Deterministic and dependency-free.
#[inline]
fn uniform_01(rng: &mut u64) -> f64 {
    *rng = rng.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *rng;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    // Top 53 bits → double in [0, 1).
    ((z >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
}

/// Seeds a SplitMix64 state from a pixel index so the dither pattern is
/// reproducible across runs.
#[inline]
fn splitmix64_seed(idx: u64) -> u64 {
    let mut z = idx.wrapping_mul(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use qdrv_core::pq::pq_oetf_f32;
    use qdrv_meta::{DynamicMeta, ToneMapCurve};

    /// Builds a `DynamicMeta` block with the linear (identity) tone curve and
    /// the given reference-display peak in nits. Used to construct the input
    /// fixtures for the tests below.
    fn fixture(ref_peak_nits: f32) -> DynamicMeta {
        let mut m = DynamicMeta::new(0, 1000.0, 200.0);
        m.target_display_hint.max_luminance_nits = ref_peak_nits;
        m.tone_map_curve = ToneMapCurve::linear();
        m
    }

    /// PQ-black must map to 8-bit black on every channel.
    #[test]
    fn sdr_black_maps_to_zero() {
        let dynamic = fixture(1000.0);
        let pixel = Pixel32::new_unchecked(0.0, 0.0, 0.0);
        let out = tone_map_to_sdr(&[pixel], &dynamic);
        assert_eq!(out[0], [0, 0, 0]);
    }

    /// A neutral sample at exactly the reference display peak must land near
    /// SDR white when the tone curve is identity. A small tolerance accounts
    /// for TPDF dither.
    #[test]
    fn sdr_reference_peak_maps_near_white() {
        let dynamic = fixture(1000.0);
        let pq_1000 = pq_oetf_f32(1000.0 / PQ_MAX_NITS as f32);
        let pixel = Pixel32::new_unchecked(pq_1000, pq_1000, pq_1000);
        let out = tone_map_to_sdr(&[pixel], &dynamic);
        for channel in out[0] {
            assert!(
                channel >= 250,
                "reference peak should map near SDR white, got {channel}"
            );
        }
    }

    /// The output vector must contain exactly one triplet per input pixel.
    #[test]
    fn sdr_output_length_matches_input() {
        let dynamic = fixture(1000.0);
        let input = vec![
            Pixel32::new_unchecked(0.0, 0.0, 0.0),
            Pixel32::new_unchecked(0.5, 0.5, 0.5),
            Pixel32::new_unchecked(1.0, 1.0, 1.0),
        ];
        let out = tone_map_to_sdr(&input, &dynamic);
        assert_eq!(out.len(), input.len());
    }

    /// Because the dither RNG is seeded from the pixel index, repeated
    /// invocations on identical input must produce identical output. This is
    /// required for bit-exact conformance testing and for reproducible
    /// rendering across runs.
    #[test]
    fn sdr_output_is_deterministic() {
        let dynamic = fixture(1000.0);
        let input: Vec<Pixel32> = (0..32)
            .map(|i| {
                let v = i as f32 / 32.0;
                Pixel32::new_unchecked(v, v, v)
            })
            .collect();
        let first = tone_map_to_sdr(&input, &dynamic);
        let second = tone_map_to_sdr(&input, &dynamic);
        assert_eq!(first, second);
    }

    /// A saturated Rec. 2020 red at 700 nits — with green and blue channels
    /// near black — must stay unmistakably red in the SDR output. The old
    /// per-channel tone-mapping path would desaturate such a pixel because
    /// the R channel is compressed far more than G and B; the luminance-
    /// preserving formulation used here avoids that hue shift.
    #[test]
    fn sdr_preserves_hue_on_saturated_red() {
        let dynamic = fixture(1000.0);
        let pq_700 = pq_oetf_f32(700.0 / PQ_MAX_NITS as f32);
        let pq_near_zero = pq_oetf_f32(0.01 / PQ_MAX_NITS as f32);
        let pixel = Pixel32::new_unchecked(pq_700, pq_near_zero, pq_near_zero);
        let out = tone_map_to_sdr(&[pixel], &dynamic);
        let [r, g, b] = out[0];
        assert!(
            r > g + 50,
            "R ({r}) should dominate G ({g}) for a saturated red"
        );
        assert!(
            r > b + 50,
            "R ({r}) should dominate B ({b}) for a saturated red"
        );
    }

    /// A neutral 50 % grey must round-trip to a neutral SDR grey: R, G, and
    /// B must all be within 1 LSB of each other after tone mapping.
    /// Combined with the hue-preservation test, this guards against regressions
    /// of the luminance-preserving pipeline.
    #[test]
    fn sdr_neutral_grey_stays_neutral() {
        let dynamic = fixture(1000.0);
        let pixel = Pixel32::new_unchecked(0.5, 0.5, 0.5);
        let out = tone_map_to_sdr(&[pixel], &dynamic);
        let [r, g, b] = out[0];
        let max = r.max(g).max(b) as i32;
        let min = r.min(g).min(b) as i32;
        assert!(max - min <= 1, "neutral grey drifted: R={r} G={g} B={b}");
    }

    /// A reference-display peak that is below the SDR white point (an edge
    /// case, but not an illegal one) must not cause division-by-zero or
    /// all-black output. The implementation raises `ref_max` to
    /// `SDR_WHITE_NITS` as a floor.
    #[test]
    fn sdr_tolerates_tiny_reference_peak() {
        let dynamic = fixture(1.0); // well below SDR_WHITE_NITS
        let pq_100 = pq_oetf_f32(100.0 / PQ_MAX_NITS as f32);
        let pixel = Pixel32::new_unchecked(pq_100, pq_100, pq_100);
        let out = tone_map_to_sdr(&[pixel], &dynamic);
        assert!(out[0].iter().all(|&c| c > 0));
    }

    /// The desaturation-to-gamut helper must leave in-gamut colours
    /// unchanged.
    #[test]
    fn desaturate_to_gamut_noop_on_in_gamut() {
        let rgb = (0.25_f64, 0.50, 0.75);
        let y = 0.2627 * rgb.0 + 0.6780 * rgb.1 + 0.0593 * rgb.2;
        let out = desaturate_to_gamut(rgb, y);
        assert!((out.0 - rgb.0).abs() < 1.0e-12);
        assert!((out.1 - rgb.1).abs() < 1.0e-12);
        assert!((out.2 - rgb.2).abs() < 1.0e-12);
    }

    /// Negative and above-unity channels must be brought back into
    /// `[0.0, 1.0]`, and the luminance of the desaturated result must equal
    /// the luminance of the input (the desaturation is luminance-preserving
    /// by construction).
    #[test]
    fn desaturate_to_gamut_fixes_out_of_gamut() {
        let rgb = (1.2_f64, 0.8, -0.1);
        let y = 0.2627 * rgb.0 + 0.6780 * rgb.1 + 0.0593 * rgb.2;
        let (r, g, b) = desaturate_to_gamut(rgb, y);
        assert!((0.0..=1.0).contains(&r), "R out of gamut: {r}");
        assert!((0.0..=1.0).contains(&g), "G out of gamut: {g}");
        assert!((0.0..=1.0).contains(&b), "B out of gamut: {b}");
        let y_out = 0.2627 * r + 0.6780 * g + 0.0593 * b;
        // Final per-channel clamp can introduce up to ~1 LSB of luminance
        // drift; 1e-6 is a comfortable tolerance for f64.
        assert!(
            (y_out - y).abs() < 1.0e-6,
            "luminance drifted: before={y} after={y_out}"
        );
    }

    /// TPDF samples must stay strictly within `(-1.0, 1.0)` (they are the
    /// difference of two independent uniform `[0, 1)` samples). This
    /// guarantees the ±½-LSB budget in `quantise_u8_tpdf` is respected.
    #[test]
    fn tpdf_sample_bounds() {
        let mut rng = splitmix64_seed(0xDEADBEEF);
        for _ in 0..10_000 {
            let s = tpdf_sample(&mut rng);
            assert!(s > -1.0 && s < 1.0, "TPDF sample out of range: {s}");
        }
    }
}
