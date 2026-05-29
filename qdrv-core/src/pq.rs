// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! SMPTE ST 2084 Perceptual Quantizer (PQ) transfer functions.
//!
//! This module implements the electro-optical transfer function (EOTF) and its
//! inverse (OETF) as defined in SMPTE ST 2084:2014, which forms the transfer
//! function basis of ITU-R Rec. 2100 (BT.2100).
//!
//! PQ encodes absolute luminance values in cd/m² (nits), mapping the range
//! `[0, 10 000]` nits to the normalised signal range `[0.0, 1.0]`. Unlike
//! gamma-based transfer functions, PQ is display-independent: a given signal
//! value always corresponds to the same absolute luminance regardless of the
//! target display.
//!
//! ## Precision policy
//!
//! All PQ computations are performed in IEEE 754 Float64 internally, even in
//! the Float32 entry points. This matches the QDRV specification requirement
//! that no integer approximations be used at any stage of the pipeline.

use crate::error::{QdrvError, Result};

// ---------------------------------------------------------------------------
// SMPTE ST 2084 PQ constants
// Sourced directly from SMPTE ST 2084:2014, Table 1.
// ---------------------------------------------------------------------------

/// Constant m1 from SMPTE ST 2084: 2610 / (4096 × 4).
const M1: f64 = 2610.0 / 4096.0 / 4.0; // 0.1593017578125

/// Constant m2 from SMPTE ST 2084: 2523 / 4096 × 128.
const M2: f64 = 2523.0 / 4096.0 * 128.0; // 78.84375

/// Constant c1 from SMPTE ST 2084: 3424 / 4096.
const C1: f64 = 3424.0 / 4096.0; // 0.8359375

/// Constant c2 from SMPTE ST 2084: 2413 / 4096 × 32.
const C2: f64 = 2413.0 / 4096.0 * 32.0; // 18.8515625

/// Constant c3 from SMPTE ST 2084: 2392 / 4096 × 32.
const C3: f64 = 2392.0 / 4096.0 * 32.0; // 18.6875

/// Maximum luminance supported by the SMPTE ST 2084 PQ transfer function,
/// in cd/m² (nits). A normalised PQ signal value of exactly `1.0` corresponds
/// to this luminance.
pub const PQ_MAX_NITS: f64 = 10_000.0;

/// Reference white luminance in cd/m² as specified in ITU-R BT.2408.
/// A PQ signal value of approximately `0.5807` corresponds to this luminance.
/// Mastering-tier content should express scene-referred values relative to
/// this reference when constructing delivery-tier output.
pub const REFERENCE_WHITE_NITS: f64 = 203.0;

// ---------------------------------------------------------------------------
// Float64 PQ functions — mastering tier
// ---------------------------------------------------------------------------

/// Applies the SMPTE ST 2084 PQ Opto-Electronic Transfer Function (OETF).
///
/// Converts a **normalised linear light** value in `[0.0, 1.0]`
/// (where `1.0` represents 10 000 nits) to a PQ-encoded signal value in
/// `[0.0, 1.0]`. All arithmetic uses IEEE 754 Float64, as required by the
/// QDRV mastering tier.
///
/// Input values outside `[0.0, 1.0]` are clamped before encoding.
#[inline]
pub fn pq_oetf_f64(linear: f64) -> f64 {
    let y = linear.clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// Applies the SMPTE ST 2084 PQ Electro-Optical Transfer Function (EOTF).
///
/// Converts a PQ-encoded signal value in `[0.0, 1.0]` to a **normalised
/// linear light** value in `[0.0, 1.0]` (where `1.0` represents 10 000 nits).
/// All arithmetic uses IEEE 754 Float64.
///
/// Input values outside `[0.0, 1.0]` are clamped before decoding.
#[inline]
pub fn pq_eotf_f64(pq: f64) -> f64 {
    let e = pq.clamp(0.0, 1.0).powf(1.0 / M2);
    let num = (e - C1).max(0.0);
    let den = C2 - C3 * e;
    (num / den).powf(1.0 / M1)
}

// ---------------------------------------------------------------------------
// Float32 PQ functions — delivery tier
// ---------------------------------------------------------------------------

/// Applies the SMPTE ST 2084 PQ OETF at Float32 precision for the
/// delivery tier.
///
/// The computation is promoted to Float64 internally for correctness, then
/// rounded back to Float32 for the delivery-tier output stream.
#[inline]
pub fn pq_oetf_f32(linear: f32) -> f32 {
    pq_oetf_f64(linear as f64) as f32
}

/// Applies the SMPTE ST 2084 PQ EOTF at Float32 precision for the
/// delivery tier.
///
/// The computation is promoted to Float64 internally for correctness, then
/// rounded back to Float32.
#[inline]
pub fn pq_eotf_f32(pq: f32) -> f32 {
    pq_eotf_f64(pq as f64) as f32
}

// ---------------------------------------------------------------------------
// Nit ↔ PQ conversions with range validation
// ---------------------------------------------------------------------------

/// Converts an absolute luminance value in nits (cd/m²) to a normalised
/// SMPTE ST 2084 PQ signal value.
///
/// # Arguments
/// * `nits` — Absolute luminance in cd/m². Must be in `[0.0, 10 000.0]`.
///
/// # Errors
/// Returns [`QdrvError::LuminanceOutOfRange`] if `nits` is outside the
/// valid PQ range.
///
/// # Example
///
/// ```
/// use qdrv_core::pq::{nits_to_pq, pq_to_nits};
///
/// // 203 nits is the ITU-R BT.2408 reference white; it encodes to
/// // ~0.5807 under SMPTE ST 2084 PQ and round-trips back to 203 nits.
/// let pq = nits_to_pq(203.0).expect("203 nits is within ST 2084 range");
/// assert!((pq - 0.5807).abs() < 1e-3);
/// let back = pq_to_nits(pq).expect("ST 2084 round-trip");
/// assert!((back - 203.0).abs() < 1e-6);
///
/// // Out-of-range inputs are rejected explicitly, not silently clamped.
/// assert!(nits_to_pq(-1.0).is_err());
/// assert!(nits_to_pq(20_000.0).is_err());
/// ```
pub fn nits_to_pq(nits: f64) -> Result<f64> {
    if !(0.0..=PQ_MAX_NITS).contains(&nits) {
        return Err(QdrvError::LuminanceOutOfRange(nits));
    }
    Ok(pq_oetf_f64(nits / PQ_MAX_NITS))
}

/// Converts a normalised SMPTE ST 2084 PQ signal value to absolute luminance
/// in nits (cd/m²).
///
/// # Arguments
/// * `pq` — Normalised PQ signal value. Must be in `[0.0, 1.0]`.
///
/// # Errors
/// Returns [`QdrvError::PqSignalOutOfRange`] if `pq` is outside `[0.0, 1.0]`.
pub fn pq_to_nits(pq: f64) -> Result<f64> {
    if !(0.0..=1.0).contains(&pq) {
        return Err(QdrvError::PqSignalOutOfRange(pq));
    }
    Ok(pq_eotf_f64(pq) * PQ_MAX_NITS)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pq_roundtrip_f64() {
        // A nit value encoded to PQ and decoded back must recover the original
        // value within a tolerance of 1e-6 nits across the full ST 2084 range.
        let test_nits = [
            0.0_f64, 0.1, 1.0, 10.0, 100.0, 203.0, 1000.0, 4000.0, 10000.0,
        ];
        for &nits in &test_nits {
            let pq = nits_to_pq(nits).unwrap();
            let recovered = pq_to_nits(pq).unwrap();
            let delta = (recovered - nits).abs();
            assert!(
                delta < 1e-6,
                "PQ roundtrip failed at {nits} nits: recovered {recovered:.9}, delta {delta:.2e}"
            );
        }
    }

    #[test]
    fn test_pq_boundaries() {
        // At linear = 0.0, the ST 2084 OETF approaches but does not produce
        // an exact zero due to the formula structure; the value must be very
        // close to zero.
        let min_pq = pq_oetf_f64(0.0);
        assert!(
            min_pq < 1e-5,
            "PQ(0.0) should be near zero, got {min_pq:.2e}"
        );

        // At linear = 1.0 (representing 10 000 nits), the OETF must produce
        // exactly 1.0.
        let max_pq = pq_oetf_f64(1.0);
        assert!(
            (max_pq - 1.0).abs() < 1e-10,
            "PQ(1.0) should be 1.0, got {max_pq:.12}"
        );
    }

    #[test]
    fn test_reference_white_pq() {
        // The ITU-R BT.2408 reference white of 203 nits must encode to
        // approximately 0.5807 under SMPTE ST 2084 PQ.
        let pq = nits_to_pq(REFERENCE_WHITE_NITS).unwrap();
        assert!(
            (pq - 0.5807).abs() < 0.001,
            "Reference white PQ value unexpected: {pq:.6} (expected ~0.5807)"
        );
    }

    #[test]
    fn test_out_of_range_errors() {
        // Values outside the valid range must produce errors, not silently
        // clamp or produce incorrect results.
        assert!(nits_to_pq(-1.0).is_err());
        assert!(nits_to_pq(10_001.0).is_err());
        assert!(pq_to_nits(-0.001).is_err());
        assert!(pq_to_nits(1.001).is_err());
    }

    #[test]
    fn test_f32_roundtrip() {
        // The Float32 entry points must round-trip within Float32 precision.
        let test_values = [0.0_f32, 0.1, 0.5, 0.5807, 0.75, 1.0];
        for &pq in &test_values {
            let linear = pq_eotf_f32(pq);
            let recovered = pq_oetf_f32(linear);
            let delta = (recovered - pq).abs();
            assert!(
                delta < 1e-5,
                "Float32 PQ roundtrip failed at PQ={pq}: recovered={recovered:.8}, delta={delta:.2e}"
            );
        }
    }
}
