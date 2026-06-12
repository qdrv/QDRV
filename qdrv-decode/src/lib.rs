// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-decode
//!
//! QDRV decoder: delivery-tier decoding and display-adaptive tone mapping.
//!
//! Applies SMPTE ST 2094-based per-frame tone mapping curves from QDRV
//! dynamic metadata to delivery-tier pixels, adapting them to the target
//! display's actual peak luminance and black level at runtime. All tone
//! mapping arithmetic uses Float32 precision.
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`tone_map`] | Display-adaptive tone mapping with v2 policy + temporal anti-pumping |
//! | [`object_tone_map`] | Per-region tone mapping override for [`qdrv_meta::ObjectMeta`] frames |
//! | [`sdr`] | Luminance-preserving SDR fallback (Rec. 709 / sRGB) with TPDF dither |
//! | [`reconstruct`] | SDR→HDR reconstruction driven by inverse tone mapping hints |
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 or later (GPLv2+).

pub mod object_tone_map;
pub mod reconstruct;
pub mod sdr;
pub mod tone_map;

pub use object_tone_map::{ObjectToneMapError, tone_map_frame_with_objects};
pub use reconstruct::{reconstruct_hdr_from_sdr, reconstruct_hdr_from_sdr_with_meta};
pub use sdr::tone_map_to_sdr;
pub use tone_map::{
    RenderMode, RenderPolicy, TargetDisplay, TemporalStateManager, tone_map_frame,
    tone_map_frame_with_policy, tone_map_frame_with_policy_and_state,
};
