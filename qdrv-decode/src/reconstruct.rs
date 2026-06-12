// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! SDR-to-HDR reconstruction helpers driven by bidirectional metadata hints.

use qdrv_core::colors::{
    ncl::{KB, KG, KR},
    srgb_to_linear,
};
use qdrv_core::pq::{PQ_MAX_NITS, pq_oetf_f32};
use qdrv_meta::{DynamicMeta, open_dynamic_v2::InverseToneMappingHint};

use qdrv_core::pixel::Pixel32;

/// Reconstructs a delivery-tier HDR frame from SDR 8-bit RGB triplets.
pub fn reconstruct_hdr_from_sdr(
    sdr_pixels: &[[u8; 3]],
    hint: &InverseToneMappingHint,
) -> Vec<Pixel32> {
    sdr_pixels
        .iter()
        .map(|rgb| reconstruct_pixel(*rgb, hint))
        .collect()
}

/// Reconstructs HDR pixels using metadata-provided inverse tone mapping hints.
pub fn reconstruct_hdr_from_sdr_with_meta(
    sdr_pixels: &[[u8; 3]],
    dynamic: &DynamicMeta,
) -> Vec<Pixel32> {
    let hint = dynamic
        .inverse_tone_mapping_hint
        .as_ref()
        .or_else(|| {
            dynamic
                .open_dynamic_v2
                .as_ref()
                .and_then(|v2| v2.inverse_tone_mapping_hint.as_ref())
        })
        .cloned()
        .unwrap_or_default();
    reconstruct_hdr_from_sdr(sdr_pixels, &hint)
}

fn reconstruct_pixel(rgb: [u8; 3], hint: &InverseToneMappingHint) -> Pixel32 {
    // Use the IEC 61966-2-1 piecewise sRGB EOTF (from `qdrv-core::colors`)
    // instead of a `^2.2` pure-gamma approximation. The piecewise form is
    // the standard for SDR-to-HDR reconstruction and avoids the small
    // black-level drift the gamma approximation introduces near 0.
    let to_lin = |v: u8| srgb_to_linear(v as f64 / 255.0) as f32;
    let mut r = to_lin(rgb[0]);
    let mut g = to_lin(rgb[1]);
    let mut b = to_lin(rgb[2]);

    let kr = KR as f32;
    let kg = KG as f32;
    let kb = KB as f32;
    let luma = kr * r + kg * g + kb * b;
    let highlight_boost = 1.0 + hint.highlight_recovery_strength * luma.powf(1.5);
    let contrast_boost = 1.0 + hint.midtone_contrast_boost * (luma * (1.0 - luma)) * 4.0;
    let sat = 1.0 + hint.saturation_compensation * 0.2;

    r = ((r - luma) * sat + luma) * highlight_boost * contrast_boost;
    g = ((g - luma) * sat + luma) * highlight_boost * contrast_boost;
    b = ((b - luma) * sat + luma) * highlight_boost * contrast_boost;

    // Map SDR-relative linear domain into a practical HDR reconstruction ceiling.
    let nits_scale = 1_000.0_f32;
    let encode = |v: f32| {
        let nits = (v.max(0.0) * nits_scale).clamp(0.0, PQ_MAX_NITS as f32);
        pq_oetf_f32(nits / PQ_MAX_NITS as f32)
    };
    Pixel32::new_unchecked(encode(r), encode(g), encode(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstruct_returns_expected_length() {
        let hint = InverseToneMappingHint::default();
        let input = vec![[128, 128, 128]; 8];
        let out = reconstruct_hdr_from_sdr(&input, &hint);
        assert_eq!(out.len(), input.len());
        assert!(
            out.iter()
                .all(|p| p.r.is_finite() && p.g.is_finite() && p.b.is_finite())
        );
    }
}
