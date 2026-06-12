// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! QDRV pixel types for both processing tiers.
//!
//! QDRV uses a two-tier pixel architecture:
//!
//! - [`Pixel64`] — the **mastering/archival tier**. Stores RGB channels as
//!   IEEE 754 Float64 linear light values in cd/m² (nits). Luminance is
//!   unbounded; values above 10 000 nits are valid and are preserved without
//!   clipping. This is the primary differentiator from all existing integer
//!   HDR formats.
//!
//! - [`Pixel32`] — the **delivery tier**. Stores RGB channels as IEEE 754
//!   Float32 values encoded under the SMPTE ST 2084 Perceptual Quantizer (PQ)
//!   transfer function. The normalised range `[0.0, 1.0]` maps to
//!   `[0, 10 000]` nits under PQ.
//!
//! - [`YCbCr32`] — a delivery-tier YCbCr representation using ITU-R Rec. 2100
//!   non-constant luminance coefficients, suitable for chroma-subsampled
//!   encoding.
//!
//! All colour primaries are ITU-R Rec. 2020, as defined by ITU-R Rec. 2100.

use crate::colors::ncl::{KB, KG, KR};
use crate::error::{QdrvError, Result};

// ---------------------------------------------------------------------------
// Mastering-tier pixel — Float64 linear light
// ---------------------------------------------------------------------------

/// A single RGB pixel in the QDRV mastering/archival tier.
///
/// All channels are stored as IEEE 754 Float64 **linear light** values
/// expressed as absolute luminance in cd/m² (nits). Unlike all existing
/// consumer HDR formats, the luminance range is **unbounded**: values above
/// 10 000 nits are valid and are preserved without clipping. This ensures
/// that the mastering master is never the precision bottleneck for any future
/// hardware generation or colour-science model.
///
/// Colour primaries: ITU-R Rec. 2020 (shared with Rec. 2100).
/// Transfer function: none — this tier stores linear light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pixel64 {
    /// Red channel, linear light, cd/m².
    pub r: f64,
    /// Green channel, linear light, cd/m².
    pub g: f64,
    /// Blue channel, linear light, cd/m².
    pub b: f64,
}

// ---------------------------------------------------------------------------
// Delivery-tier pixel — Float32 PQ-encoded
// ---------------------------------------------------------------------------

/// A single RGB pixel in the QDRV delivery tier.
///
/// All channels are stored as IEEE 754 Float32 values encoded under the
/// SMPTE ST 2084 Perceptual Quantizer (PQ) transfer function, as specified
/// in ITU-R Rec. 2100. The normalised range `[0.0, 1.0]` maps to
/// `[0, 10 000]` nits under PQ.
///
/// Colour standard: ITU-R Rec. 2100 (BT.2100).
/// Transfer function: SMPTE ST 2084 PQ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pixel32 {
    /// Red channel, PQ-encoded, `[0.0, 1.0]`.
    pub r: f32,
    /// Green channel, PQ-encoded, `[0.0, 1.0]`.
    pub g: f32,
    /// Blue channel, PQ-encoded, `[0.0, 1.0]`.
    pub b: f32,
}

// ---------------------------------------------------------------------------
// YCbCr delivery pixel — Rec. 2100 non-constant luminance
// ---------------------------------------------------------------------------

/// A YCbCr pixel for QDRV delivery-tier encoded streams.
///
/// Uses the ITU-R Rec. 2100 non-constant luminance (NCL) coefficients:
/// - KR = 0.2627
/// - KG = 0.6780
/// - KB = 0.0593
///
/// The luma component `Y` is in `[0.0, 1.0]`.
/// The chroma components `Cb` and `Cr` are in `[-0.5, 0.5]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YCbCr32 {
    /// Luma component, `[0.0, 1.0]`.
    pub y: f32,
    /// Blue-difference chroma, `[-0.5, 0.5]`.
    pub cb: f32,
    /// Red-difference chroma, `[-0.5, 0.5]`.
    pub cr: f32,
}

// ---------------------------------------------------------------------------
// Pixel64 implementation
// ---------------------------------------------------------------------------

impl Pixel64 {
    /// Creates a new mastering-tier pixel, validating that all channels are
    /// finite. Negative values are permitted to allow out-of-gamut
    /// representation during compositing operations.
    ///
    /// # Errors
    /// Returns [`QdrvError::NonFiniteValue`] if any channel is NaN or infinite.
    pub fn new(r: f64, g: f64, b: f64) -> Result<Self> {
        if !r.is_finite() {
            return Err(QdrvError::NonFiniteValue(r));
        }
        if !g.is_finite() {
            return Err(QdrvError::NonFiniteValue(g));
        }
        if !b.is_finite() {
            return Err(QdrvError::NonFiniteValue(b));
        }
        Ok(Self { r, g, b })
    }

    /// Creates a new mastering-tier pixel without validation.
    ///
    /// The caller is responsible for ensuring all values are finite.
    /// Prefer [`Pixel64::new`] in any context where the input source is not
    /// fully trusted.
    #[inline]
    pub const fn new_unchecked(r: f64, g: f64, b: f64) -> Self {
        Self { r, g, b }
    }

    /// Computes the absolute luminance of this pixel in nits using the
    /// ITU-R Rec. 2100 non-constant luminance coefficients.
    ///
    /// The formula applied is: `L = KR·R + KG·G + KB·B` with the
    /// Rec. 2100 NCL coefficients in [`crate::colors::ncl`].
    #[inline]
    pub fn luminance_nits(&self) -> f64 {
        KR * self.r + KG * self.g + KB * self.b
    }

    /// Downcasts to a Float32 delivery-tier pixel by truncating precision.
    ///
    /// This method performs a raw precision downcast only. It does **not**
    /// apply the SMPTE ST 2084 PQ transfer function. For a complete
    /// mastering-to-delivery transcode, use `qdrv_encode::transcode_frame`.
    #[inline]
    pub fn downcast_raw(&self) -> Pixel32 {
        Pixel32 {
            r: self.r as f32,
            g: self.g as f32,
            b: self.b as f32,
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel32 implementation
// ---------------------------------------------------------------------------

impl Pixel32 {
    /// Creates a new delivery-tier pixel, validating that all channels are
    /// finite.
    ///
    /// # Errors
    /// Returns [`QdrvError::NonFiniteValue`] if any channel is NaN or infinite.
    pub fn new(r: f32, g: f32, b: f32) -> Result<Self> {
        if !r.is_finite() {
            return Err(QdrvError::NonFiniteValue(r as f64));
        }
        if !g.is_finite() {
            return Err(QdrvError::NonFiniteValue(g as f64));
        }
        if !b.is_finite() {
            return Err(QdrvError::NonFiniteValue(b as f64));
        }
        Ok(Self { r, g, b })
    }

    /// Creates a new delivery-tier pixel without validation.
    ///
    /// The caller is responsible for ensuring all values are finite.
    #[inline]
    pub const fn new_unchecked(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    /// Computes the ITU-R Rec. 2100 NCL luma **of the PQ-encoded signal
    /// values** of this pixel.
    ///
    /// **Not absolute luminance.** The NCL coefficients (0.2627, 0.6780,
    /// 0.0593) are defined for linear-light values; applying them directly
    /// to PQ-encoded values yields a perceptual luma proxy, not a luminance
    /// in nits. To obtain absolute luminance, decode the channels with
    /// [`crate::pq::pq_eotf_f32`] first.
    ///
    /// This method is named `pq_signal_luma` rather than `pq_luminance` to
    /// avoid that confusion at call sites.
    #[inline]
    pub fn pq_signal_luma(&self) -> f32 {
        (KR as f32) * self.r + (KG as f32) * self.g + (KB as f32) * self.b
    }

    /// Upcasts this pixel to Float64 for high-precision processing.
    ///
    /// No transfer function is applied or removed; this is a raw precision
    /// upcast only. The resulting `Pixel64` remains in the PQ-encoded domain
    /// unless the caller explicitly applies the ST 2084 EOTF.
    #[inline]
    pub fn upcast_raw(&self) -> Pixel64 {
        Pixel64 {
            r: self.r as f64,
            g: self.g as f64,
            b: self.b as f64,
        }
    }
}

// ---------------------------------------------------------------------------
// YCbCr32 implementation
// ---------------------------------------------------------------------------

// Rec. 2100 NCL chroma scaling factors derived from the luma coefficients:
//   2·(1 − KR) for Cr↔R   →  2·(1 − 0.2627) = 1.4746
//   2·(1 − KB) for Cb↔B   →  2·(1 − 0.0593) = 1.8814
// Computed at compile time from `crate::colors::ncl` so these YCbCr
// transforms can never drift away from the centralised KR/KG/KB constants
// used by every other luma site in the workspace.
const TWO_ONE_MINUS_KR_F32: f32 = 2.0 * (1.0 - KR as f32);
const TWO_ONE_MINUS_KB_F32: f32 = 2.0 * (1.0 - KB as f32);

impl YCbCr32 {
    /// Converts this ITU-R Rec. 2100 non-constant luminance YCbCr pixel to
    /// an RGB [`Pixel32`].
    ///
    /// The inverse transform coefficients used are derived from the
    /// Rec. 2100 NCL constants in [`crate::colors::ncl`]:
    /// - R = Y + 2·(1 − KR)·Cr
    /// - B = Y + 2·(1 − KB)·Cb
    /// - G = (Y − KR·R − KB·B) / KG
    pub fn to_rgb(&self) -> Pixel32 {
        let kr = KR as f32;
        let kg = KG as f32;
        let kb = KB as f32;
        let r = self.y + TWO_ONE_MINUS_KR_F32 * self.cr;
        let b = self.y + TWO_ONE_MINUS_KB_F32 * self.cb;
        let g = (self.y - kr * r - kb * b) / kg;
        Pixel32::new_unchecked(r, g, b)
    }
}

impl From<Pixel32> for YCbCr32 {
    /// Converts a PQ-encoded RGB [`Pixel32`] to ITU-R Rec. 2100
    /// non-constant luminance YCbCr.
    ///
    /// The forward transform coefficients applied are derived from the
    /// Rec. 2100 NCL constants in [`crate::colors::ncl`]:
    /// - Y  =  KR·R + KG·G + KB·B
    /// - Cb = (B − Y) / (2·(1 − KB))
    /// - Cr = (R − Y) / (2·(1 − KR))
    fn from(p: Pixel32) -> Self {
        let kr = KR as f32;
        let kg = KG as f32;
        let kb = KB as f32;
        let y = kr * p.r + kg * p.g + kb * p.b;
        let cb = (p.b - y) / TWO_ONE_MINUS_KB_F32;
        let cr = (p.r - y) / TWO_ONE_MINUS_KR_F32;
        Self { y, cb, cr }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pixel64_luminance_white() {
        // A neutral grey at 203 nits (the ITU-R BT.2408 reference white)
        // should report exactly 203 nits of luminance.
        let p = Pixel64::new_unchecked(203.0, 203.0, 203.0);
        let lum = p.luminance_nits();
        assert!((lum - 203.0).abs() < 1e-9, "Expected 203.0 nits, got {lum}");
    }

    #[test]
    fn test_ycbcr_rgb_roundtrip() {
        // Converting RGB → YCbCr → RGB must recover the original values
        // within Float32 precision.
        let original = Pixel32::new_unchecked(0.5, 0.3, 0.7);
        let ycbcr = YCbCr32::from(original);
        let recovered = ycbcr.to_rgb();
        assert!((recovered.r - original.r).abs() < 1e-5);
        assert!((recovered.g - original.g).abs() < 1e-5);
        assert!((recovered.b - original.b).abs() < 1e-5);
    }

    #[test]
    fn test_pixel64_downcast_upcast() {
        // A raw downcast followed by a raw upcast must recover the original
        // value within the precision loss expected of Float32.
        let p64 = Pixel64::new_unchecked(1000.0, 500.0, 200.0);
        let p32 = p64.downcast_raw();
        let back = p32.upcast_raw();
        assert!((back.r - 1000.0).abs() < 0.1);
        assert!((back.g - 500.0).abs() < 0.1);
        assert!((back.b - 200.0).abs() < 0.1);
    }

    #[test]
    fn test_pixel_non_finite_rejected() {
        // The validated constructors must reject NaN and infinite values.
        assert!(Pixel64::new(f64::NAN, 0.0, 0.0).is_err());
        assert!(Pixel64::new(0.0, f64::INFINITY, 0.0).is_err());
        assert!(Pixel32::new(f32::NAN, 0.0, 0.0).is_err());
    }
}
