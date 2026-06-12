// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Fidelity metric helpers used by quality contracts and conformance checks.

use crate::{
    colors::{
        REC2020_TO_XYZ, apply_matrix,
        ncl::{KB, KG, KR},
    },
    pixel::Pixel32,
    pq::pq_eotf_f32,
};

/// Core frame-level fidelity metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameFidelityMetrics {
    /// Peak signal-to-noise ratio in dB.
    pub psnr_db: f64,
    /// Structural similarity index, [0, 1].
    pub ssim: f64,
    /// Mean CIE DeltaE76 in Lab space.
    pub delta_e76: f64,
}

/// Computes PSNR for two scalar signals with known peak value.
pub fn compute_psnr(reference: &[f32], candidate: &[f32], peak: f32) -> Option<f64> {
    if reference.is_empty()
        || reference.len() != candidate.len()
        || peak <= 0.0
        || !peak.is_finite()
    {
        return None;
    }
    let mse = reference
        .iter()
        .zip(candidate)
        .map(|(&r, &c)| {
            let d = (r - c) as f64;
            d * d
        })
        .sum::<f64>()
        / reference.len() as f64;
    if mse <= f64::EPSILON {
        return Some(f64::INFINITY);
    }
    let peak = peak as f64;
    Some(10.0 * ((peak * peak) / mse).log10())
}

/// Computes the **global** (single-window) SSIM between two scalar signals.
///
/// This is not the windowed reference SSIM (Wang et al. 2004) that averages
/// per-window scores across the image — it is the cheaper single-statistic
/// variant that treats the whole frame as one window. It is suitable for
/// quick-look quality gates but should not be reported as "reference SSIM"
/// without qualification.
pub fn compute_ssim(reference: &[f32], candidate: &[f32]) -> Option<f64> {
    if reference.is_empty() || reference.len() != candidate.len() {
        return None;
    }
    let n = reference.len() as f64;
    let mean_r = reference.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mean_c = candidate.iter().map(|&v| v as f64).sum::<f64>() / n;

    let mut var_r = 0.0_f64;
    let mut var_c = 0.0_f64;
    let mut cov = 0.0_f64;
    for (&r, &c) in reference.iter().zip(candidate) {
        let dr = r as f64 - mean_r;
        let dc = c as f64 - mean_c;
        var_r += dr * dr;
        var_c += dc * dc;
        cov += dr * dc;
    }

    let denom = (n - 1.0).max(1.0);
    var_r /= denom;
    var_c /= denom;
    cov /= denom;

    let c1 = 0.01f64.powi(2);
    let c2 = 0.03f64.powi(2);
    let numerator = (2.0 * mean_r * mean_c + c1) * (2.0 * cov + c2);
    let denominator = (mean_r * mean_r + mean_c * mean_c + c1) * (var_r + var_c + c2);
    if denominator <= f64::EPSILON {
        return None;
    }
    Some((numerator / denominator).clamp(0.0, 1.0))
}

/// Computes the average CIE ΔE76 between two **linear-light** Rec. 2020 RGB
/// signals.
///
/// The inputs must be linear light, **not** PQ-encoded: the CIELAB pipeline
/// (`REC2020_TO_XYZ` → `xyz_to_lab`) is only colorimetrically valid on linear
/// tristimulus values. Callers holding ST 2084 PQ signals must decode them
/// with [`crate::pq::pq_eotf_f32`] first — see [`metrics_for_delivery_frame`],
/// which does exactly that before calling this function.
pub fn compute_delta_e76(reference: &[[f32; 3]], candidate: &[[f32; 3]]) -> Option<f64> {
    if reference.is_empty() || reference.len() != candidate.len() {
        return None;
    }
    let sum = reference
        .iter()
        .zip(candidate)
        .map(|(r, c)| {
            let lab_r = rec2020_to_lab(r);
            let lab_c = rec2020_to_lab(c);
            let dl = lab_r.0 - lab_c.0;
            let da = lab_r.1 - lab_c.1;
            let db = lab_r.2 - lab_c.2;
            (dl * dl + da * da + db * db).sqrt()
        })
        .sum::<f64>();
    Some(sum / reference.len() as f64)
}

/// Computes PSNR/SSIM/ΔE76 for two delivery-tier frames.
///
/// PSNR and SSIM are evaluated on the Rec. 2100 NCL luma of the **PQ signal**,
/// which is the correct domain for signal-fidelity metrics. ΔE76 is evaluated
/// on the **PQ-decoded linear light** of each pixel, because CIELAB is only
/// meaningful on linear tristimulus values; the reference white is the
/// 10 000-nit ST 2084 ceiling (a linear value of `1.0`).
pub fn metrics_for_delivery_frame(
    reference: &[Pixel32],
    candidate: &[Pixel32],
) -> Option<FrameFidelityMetrics> {
    if reference.is_empty() || reference.len() != candidate.len() {
        return None;
    }

    let mut ref_luma = Vec::with_capacity(reference.len());
    let mut cand_luma = Vec::with_capacity(candidate.len());
    let mut ref_rgb = Vec::with_capacity(reference.len());
    let mut cand_rgb = Vec::with_capacity(candidate.len());

    let kr = KR as f32;
    let kg = KG as f32;
    let kb = KB as f32;
    for (r, c) in reference.iter().zip(candidate) {
        let yr = kr * r.r + kg * r.g + kb * r.b;
        let yc = kr * c.r + kg * c.g + kb * c.b;
        ref_luma.push(yr.clamp(0.0, 1.0));
        cand_luma.push(yc.clamp(0.0, 1.0));
        // ΔE76 is a CIELAB metric and is only meaningful on linear-light
        // tristimulus values. These channels are ST 2084 PQ-encoded, so decode
        // them to normalised linear light (1.0 ≡ 10 000 nits) before the
        // Rec. 2020 → XYZ → Lab conversion inside `compute_delta_e76`. The
        // luma fed to PSNR/SSIM above intentionally stays in the PQ signal
        // domain, which is the correct domain for those signal-fidelity
        // metrics.
        ref_rgb.push([pq_eotf_f32(r.r), pq_eotf_f32(r.g), pq_eotf_f32(r.b)]);
        cand_rgb.push([pq_eotf_f32(c.r), pq_eotf_f32(c.g), pq_eotf_f32(c.b)]);
    }

    Some(FrameFidelityMetrics {
        psnr_db: compute_psnr(&ref_luma, &cand_luma, 1.0)?,
        ssim: compute_ssim(&ref_luma, &cand_luma)?,
        delta_e76: compute_delta_e76(&ref_rgb, &cand_rgb)?,
    })
}

fn rec2020_to_lab(rgb: &[f32; 3]) -> (f64, f64, f64) {
    let xyz = apply_matrix(
        (rgb[0] as f64, rgb[1] as f64, rgb[2] as f64),
        &REC2020_TO_XYZ,
    );
    xyz_to_lab(xyz.0, xyz.1, xyz.2)
}

fn xyz_to_lab(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    // D65 white point.
    let xr = x / 0.95047;
    let yr = y / 1.00000;
    let zr = z / 1.08883;

    let fx = f_lab(xr);
    let fy = f_lab(yr);
    let fz = f_lab(zr);

    let l = 116.0 * fy - 16.0;
    let a = 500.0 * (fx - fy);
    let b = 200.0 * (fy - fz);
    (l, a, b)
}

// CIE 1976 L*a*b* piecewise transform constants, exact rational forms.
// `EPSILON_LAB = (6/29)^3 = 216/24389` and `KAPPA_LAB = (29/3)^3 / 116 = 841/108`
// per CIE 15:2004 §8.2.1.1. The truncated decimals `0.008856` / `7.787`
// commonly seen in textbooks drift slightly at the segment boundary; using
// the rationals keeps the transition smooth to f64 precision.
const EPSILON_LAB: f64 = 216.0 / 24389.0;
const KAPPA_LAB: f64 = 841.0 / 108.0;
const OFFSET_LAB: f64 = 4.0 / 29.0;

#[inline]
fn f_lab(v: f64) -> f64 {
    if v > EPSILON_LAB {
        v.cbrt()
    } else {
        KAPPA_LAB * v + OFFSET_LAB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_identity_is_perfect() {
        let frame = vec![Pixel32::new_unchecked(0.2, 0.3, 0.4); 16];
        let metrics = metrics_for_delivery_frame(&frame, &frame).unwrap();
        assert!(metrics.psnr_db.is_infinite());
        assert!((metrics.ssim - 1.0).abs() < 1e-12);
        assert!(metrics.delta_e76 < 1e-9);
    }

    #[test]
    fn psnr_detects_error() {
        let ref_sig = vec![0.0, 0.5, 1.0, 0.75];
        let cand_sig = vec![0.0, 0.5, 0.9, 0.75];
        let psnr = compute_psnr(&ref_sig, &cand_sig, 1.0).unwrap();
        assert!(psnr.is_finite());
        assert!(psnr < 50.0);
    }

    /// Regression guard for the ΔE76 domain fix: `metrics_for_delivery_frame`
    /// must decode the ST 2084 PQ channels to linear light before the CIELAB
    /// conversion, not feed the PQ signal straight into the linear-light
    /// matrix. The previous code did the latter, which is not a valid ΔE76.
    #[test]
    fn delta_e76_is_computed_in_linear_light_not_pq_signal() {
        // Two single-pixel frames differing only in the red PQ channel.
        let a = Pixel32::new_unchecked(0.6, 0.5, 0.5);
        let b = Pixel32::new_unchecked(0.5, 0.5, 0.5);
        let metrics = metrics_for_delivery_frame(&[a], &[b]).unwrap();

        // The reported ΔE76 must equal `compute_delta_e76` applied to the
        // PQ-decoded (linear-light) channels — i.e. the decode happened.
        let lin_a = [pq_eotf_f32(a.r), pq_eotf_f32(a.g), pq_eotf_f32(a.b)];
        let lin_b = [pq_eotf_f32(b.r), pq_eotf_f32(b.g), pq_eotf_f32(b.b)];
        let expected_linear = compute_delta_e76(&[lin_a], &[lin_b]).unwrap();
        assert!(
            (metrics.delta_e76 - expected_linear).abs() < 1e-9,
            "ΔE76 must be computed on PQ-decoded linear light: got {}, expected {}",
            metrics.delta_e76,
            expected_linear
        );

        // And it must differ from the old (incorrect) signal-domain value,
        // guarding against a regression back to feeding PQ signals straight
        // into the linear-light CIELAB pipeline.
        let signal_domain = compute_delta_e76(&[[a.r, a.g, a.b]], &[[b.r, b.g, b.b]]).unwrap();
        assert!(
            (metrics.delta_e76 - signal_domain).abs() > 1e-6,
            "linear-light ΔE76 ({}) should differ from the PQ-signal-domain value ({})",
            metrics.delta_e76,
            signal_domain
        );
    }
}
