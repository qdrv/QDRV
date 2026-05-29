// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-codec
//!
//! Codec layer for **QDRV — Quantum Dynamic Range Video**.
//!
//! | Module | Tier | Codec | Notes |
//! |--------|------|-------|-------|
//! | [`av1`] | Delivery (Float32 PQ) | AV1 12-bit 4:4:4 via rav1e + dav1d | Always available |
//! | [`compress`] | Mastering (Float64 linear) | fpzip | Default, pure Rust |
//! | [`compress`] | Mastering (Float64 linear) | ZFP reversible | Optional, `zfp` feature |
//! | [`temporal`] | Delivery (Float32 PQ) | AV1 GOP encoder with inter-frame prediction | Always available |
//! | [`error`] | n/a | [`CodecError`] error type | Always available |
//!
//! ## Why not zstd for mastering?
//!
//! zstd is a general-purpose LZ77+ANS compressor that is unaware of the
//! structure of IEEE 754 floating-point data. fpzip and ZFP both exploit the
//! specific bit-layout of floating-point values and the spatial correlation
//! of adjacent image pixels, achieving significantly better compression ratios
//! on floating-point image data than zstd.
//!
//! ## Why not SZ3?
//!
//! SZ3 is a primarily **lossy** compressor designed for scientific simulations
//! where small controlled errors are acceptable. QDRV mastering frames require
//! bit-for-bit exact preservation of every Float64 value, and SZ3's lossless
//! mode offers no advantage over fpzip or ZFP for smooth floating-point image data.
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 (GPLv2).

pub mod av1;
pub mod compress;
pub mod error;
pub mod temporal;

pub use av1::{
    Av1Config, Av1Decoder, ChromaSampling420, decode_frame as av1_decode,
    encode_frame as av1_encode,
};
pub use compress::{
    MASTERING_CODEC_FPZIP, MASTERING_CODEC_ZFP, MasteringCodec,
    compress_frame as mastering_compress, decompress_frame as mastering_decompress,
};
pub use error::CodecError;
pub use temporal::{EncodedPacket, GopConfig, TemporalEncoder};
