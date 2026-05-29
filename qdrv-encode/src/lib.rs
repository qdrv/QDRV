// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-encode
//!
//! QDRV encoder: mastering-to-delivery tier transcoding.
//!
//! Converts Float64 linear light mastering-tier pixels to Float32 SMPTE
//! ST 2084 PQ-encoded delivery-tier pixels, and generates SMPTE ST 2094-based
//! per-frame dynamic metadata containing scene luminance statistics and
//! tone mapping curves.
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 (GPLv2).

pub mod transcode;

pub use transcode::{
    EncodeError, EncodeOptions, TranscodeResult, to_hdr10_10bit, transcode_frame,
    transcode_frame_with_options,
};
