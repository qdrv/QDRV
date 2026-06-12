// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Error types for `qdrv-io` file reading and writing operations.

use thiserror::Error;

/// All errors that can be produced by `qdrv-io` operations.
#[derive(Debug, Error)]
pub enum IoError {
    /// An underlying I/O error from the operating system or stream.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The file does not begin with the QDRV magic bytes (`QDRV`).
    #[error("invalid magic bytes: file is not a QDRV container")]
    InvalidMagic,

    /// The file declares an unknown or deprecated format version that this
    /// implementation does not support.
    #[error("unsupported QDRV container version: {0}")]
    UnsupportedVersion(u16),

    /// The file declares a container version newer than this implementation
    /// understands.
    #[error("future QDRV container version encountered: {0}")]
    FutureVersion(u16),

    /// The tier byte in the file header is not a known value (`0` or `1`).
    #[error("invalid tier byte in file header: {0}")]
    InvalidTier(u8),

    /// The codec byte in the file header is not a known value (`0` or `1`).
    #[error("invalid codec byte in file header: {0}")]
    InvalidCodec(u8),

    /// A metadata block in the file could not be parsed as valid JSON or
    /// deserialised into the expected QDRV metadata type.
    #[error("invalid metadata block: {0}")]
    InvalidMetadata(String),

    /// A declared size from untrusted input exceeded a configured hard limit.
    #[error("declared {context} length {declared} exceeds maximum {maximum}")]
    SizeLimitExceeded {
        /// Logical block name that exceeded bounds (metadata, pixel payload, etc.).
        context: &'static str,
        /// Declared byte/pixel length from the input.
        declared: usize,
        /// Maximum accepted value for this context.
        maximum: usize,
    },

    /// Integer arithmetic overflow while computing a size derived from input.
    #[error("size computation overflow while evaluating {context}")]
    SizeOverflow {
        /// Logical computation that overflowed.
        context: &'static str,
    },

    /// Allocation request failed after passing explicit bounds checks.
    #[error("allocation failed for {context}: requested {requested} bytes")]
    AllocationFailed {
        /// Logical block being allocated.
        context: &'static str,
        /// Requested allocation size in bytes.
        requested: usize,
    },

    /// A pixel data block was shorter than the expected byte count.
    #[error("truncated pixel data in frame {frame}: expected {expected} bytes: {source}")]
    TruncatedPixelData {
        /// Zero-based frame index where the truncation occurred.
        frame: usize,
        /// Expected byte count.
        expected: usize,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// A frame passed to the writer contained a different number of pixels
    /// than `width × height`.
    #[error("pixel count mismatch in frame {frame}: expected {expected} pixels, got {actual}")]
    PixelCountMismatch {
        /// Zero-based frame index.
        frame: usize,
        /// Expected pixel count (`width × height`).
        expected: usize,
        /// Actual pixel count in the buffer.
        actual: usize,
    },

    /// A metadata value could not be serialised to JSON for writing.
    #[error("metadata serialisation failed: {0}")]
    MetaSerialisationFailed(String),

    /// The caller requested an output container version that cannot be emitted.
    #[error("unsupported QDRV output container version: {0}")]
    UnsupportedWriteVersion(u16),

    /// A codec operation (AV1 encode/decode or fpzip/ZFP compress/decompress) failed.
    #[error("codec error in frame {frame}: {message}")]
    Codec {
        /// Zero-based frame index where the codec error occurred.
        frame: usize,
        /// Error message from the codec layer.
        message: String,
    },
}
