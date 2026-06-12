// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-core
//!
//! Core types and functions for **QDRV — Quantum Dynamic Range Video**.
//!
//! This crate provides the mathematical and type-level foundation of the QDRV
//! format. All other QDRV crates depend on it. The following modules are
//! exposed:
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`pixel`] | [`Pixel64`] (mastering tier), [`Pixel32`] (delivery tier), [`YCbCr32`] |
//! | [`pq`] | SMPTE ST 2084 PQ transfer functions and nit/PQ conversions |
//! | [`colors`] | ITU-R Rec. 2100 / Rec. 2020 colour matrices and gamut transforms |
//! | [`aces`] | ACES AP0 / AP1 ↔ Rec. 2020 transforms with D60→D65 chromatic adaptation |
//! | [`fidelity`] | Frame-level PSNR, SSIM, and ΔE76 fidelity metrics |
//! | [`error`] | [`QdrvError`] and [`Result`] alias |
//!
//! ## Standards implemented
//!
//! - **ITU-R Rec. 2100 (BT.2100)** — HDR television picture parameter standard
//! - **SMPTE ST 2084** — Perceptual Quantizer (PQ) transfer function
//! - **ITU-R Rec. 2020 (BT.2020)** — Wide-gamut colour primaries
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 (GPLv2).

pub mod aces;
pub mod colors;
pub mod error;
pub mod fidelity;
pub mod pixel;
pub mod pq;

pub use aces::{
    ACES_AP0_TO_REC2020, ACES_AP1_TO_REC2020, REC2020_TO_ACES_AP0, REC2020_TO_ACES_AP1,
    aces_ap0_to_rec2020, aces_ap1_to_rec2020, apply_odt_rec709_100nit, apply_odt_rec2020_1000nit,
    apply_odt_rec2020_4000nit, apply_rrt, rec2020_to_aces_ap0, rec2020_to_aces_ap1,
};
pub use colors::{linear_to_srgb, srgb_to_linear};
pub use error::{QdrvError, Result};
pub use fidelity::{
    FrameFidelityMetrics, compute_delta_e76, compute_psnr, compute_ssim, metrics_for_delivery_frame,
};
pub use pixel::{Pixel32, Pixel64, YCbCr32};
pub use pq::{PQ_MAX_NITS, REFERENCE_WHITE_NITS};
