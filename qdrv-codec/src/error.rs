// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Error types for `qdrv-codec` operations.

use thiserror::Error;

/// All errors that can be produced by `qdrv-codec` operations.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The rav1e AV1 encoder rejected the configuration or reported a fatal
    /// error during encoding.
    #[error("AV1 encode error: {0}")]
    Av1Encode(String),

    /// The dav1d AV1 decoder reported an error during decoding.
    #[error("AV1 decode error: {0}")]
    Av1Decode(String),

    /// The rav1e encoder produced no output packets after flushing.
    #[error("AV1 encoder produced no output packets for frame {0}")]
    NoPacketsProduced(u64),

    /// The dav1d decoder produced no output picture from the bitstream.
    #[error("AV1 decoder produced no picture from bitstream")]
    NoPictureDecoded,

    /// The decoded picture has different dimensions than expected.
    #[error(
        "decoded picture dimensions mismatch: expected {expected_w}×{expected_h}, \
         got {actual_w}×{actual_h}"
    )]
    PictureDimensionMismatch {
        expected_w: u32,
        expected_h: u32,
        actual_w: u32,
        actual_h: u32,
    },

    /// The decoded pixel buffer contained a different number of pixels than expected.
    #[error("pixel count mismatch after decode: expected {expected}, got {actual}")]
    PixelCountMismatch { expected: usize, actual: usize },

    /// An fpzip compression or decompression operation failed.
    #[error("fpzip error: {0}")]
    Fpzip(String),

    /// A ZFP compression or decompression operation failed.
    /// Only produced when the `zfp` feature is enabled.
    #[error("ZFP error: {0}")]
    Zfp(String),

    /// The decompressed byte buffer had a length that is not divisible by the
    /// expected per-pixel byte count.
    #[error(
        "decompressed mastering frame has {byte_count} bytes, \
         not divisible by {bytes_per_pixel} (expected {expected_pixels} pixels)"
    )]
    MalformedMasteringFrame {
        byte_count: usize,
        bytes_per_pixel: usize,
        expected_pixels: usize,
    },

    /// The codec identifier byte at the start of a mastering frame blob is not
    /// a recognised QDRV mastering codec.
    #[error("unrecognised mastering codec byte: {0}")]
    UnknownMasteringCodec(u8),
}
