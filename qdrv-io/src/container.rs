// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! QDRV binary container format definitions.
//!
//! ## Container layout
//!
//! The QDRV container format uses a fixed 28-byte header followed by a static
//! metadata block and then per-frame blocks. Explicit container versions `1`
//! and `2` are supported by this implementation.
//!
//! - Writers default to version `2`.
//! - Readers accept both versions `1` and `2`.
//! - Writers can still emit version `1` when explicitly requested.
//!
//! Two codec modes are defined:
//!
//! | Codec byte | Pixel storage |
//! |------------|--------------|
//! | 0 | Raw IEEE 754 bytes (uncompressed, for testing only) |
//! | 1 | AV1 (delivery) / fpzip or ZFP (mastering) |
//!
//! ## File header layout (28 bytes)
//!
//! | Offset | Size | Type | Field |
//! |--------|------|------|-------|
//! | 0 | 4 | `[u8; 4]` | Magic: `QDRV` (0x51 0x44 0x52 0x56) |
//! | 4 | 2 | `u16 LE` | Container format version (`1` or `2`) |
//! | 6 | 1 | `u8` | Tier: `0` = mastering (Float64), `1` = delivery (Float32) |
//! | 7 | 1 | `u8` | Codec: `0` = raw (uncompressed), `1` = AV1/fpzip+ZFP |
//! | 8 | 4 | `u32 LE` | Frame width in pixels |
//! | 12 | 4 | `u32 LE` | Frame height in pixels |
//! | 16 | 4 | `u32 LE` | Number of frames |
//! | 20 | 4 | `u32 LE` | Flags (reserved; must be 0) |
//! | 24 | 4 | `u32 LE` | Byte length of static metadata JSON block |
//!
//! ## Per-frame blocks (codec=1)
//!
//! For each frame, in order:
//! 1. Dynamic metadata length (`u32 LE`)
//! 2. Dynamic metadata JSON (UTF-8)
//! 3. Pixel data length (`u32 LE`)
//! 4. Pixel data:
//!    - Delivery: AV1 still-picture bitstream
//!    - Mastering: fpzip or ZFP blob with leading codec identifier byte

use crate::error::IoError;

/// QDRV container magic bytes: ASCII "QDRV".
pub const MAGIC: [u8; 4] = *b"QDRV";

/// QDRV container format version 1 (legacy-compatible).
pub const CONTAINER_VERSION_V1: u16 = 1;
/// QDRV container format version 2 (current writer default).
pub const CONTAINER_VERSION_V2: u16 = 2;
/// Minimum container version accepted by readers.
pub const MIN_SUPPORTED_CONTAINER_VERSION: u16 = CONTAINER_VERSION_V1;
/// Maximum container version accepted by readers.
pub const MAX_SUPPORTED_CONTAINER_VERSION: u16 = CONTAINER_VERSION_V2;
/// Current QDRV container format version written by this implementation.
pub const CURRENT_FORMAT_VERSION: u16 = CONTAINER_VERSION_V2;

/// Tier byte: mastering/archival tier (Float64 linear light).
pub const TIER_MASTERING: u8 = 0;

/// Tier byte: delivery tier (Float32 SMPTE ST 2084 PQ).
pub const TIER_DELIVERY: u8 = 1;

/// Codec byte for format version 1: raw uncompressed IEEE 754 bytes.
pub const CODEC_RAW: u8 = 0;

/// Codec byte for the production codec: AV1 (delivery) / fpzip or ZFP (mastering).
/// The mastering per-blob codec byte distinguishes fpzip from ZFP at frame level.
pub const CODEC_AV1: u8 = 1;
/// Alias for codec byte `1`: compressed payload where tier selects codec family.
///
/// Retained alongside `CODEC_AV1` for backwards compatibility and existing call sites.
pub const CODEC_COMPRESSED: u8 = CODEC_AV1;

/// Size of the fixed-length file header in bytes.
pub const HEADER_SIZE: usize = 28;

/// Returns `true` if a container version is supported by this implementation.
pub fn is_supported_container_version(version: u16) -> bool {
    matches!(version, CONTAINER_VERSION_V1 | CONTAINER_VERSION_V2)
}

/// Returns `true` if a container version is newer than the known maximum.
pub fn is_future_container_version(version: u16) -> bool {
    version > MAX_SUPPORTED_CONTAINER_VERSION
}

/// Parsed QDRV file header.
#[derive(Debug, Clone, PartialEq)]
pub struct FileHeader {
    /// Container format version (`1` or `2`).
    pub version: u16,
    /// Tier byte (`0` = mastering, `1` = delivery).
    pub tier: u8,
    /// Codec byte (`0` = raw, `1` = AV1/fpzip+ZFP).
    pub codec: u8,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Number of frames in the file.
    pub frame_count: u32,
    /// Byte length of the static metadata JSON block that follows the header.
    pub static_meta_len: u32,
}

impl FileHeader {
    /// Serialises this header to the 28-byte QDRV binary layout.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6] = self.tier;
        buf[7] = self.codec;
        buf[8..12].copy_from_slice(&self.width.to_le_bytes());
        buf[12..16].copy_from_slice(&self.height.to_le_bytes());
        buf[16..20].copy_from_slice(&self.frame_count.to_le_bytes());
        buf[20..24].copy_from_slice(&0u32.to_le_bytes()); // Flags, reserved.
        buf[24..28].copy_from_slice(&self.static_meta_len.to_le_bytes());
        buf
    }

    /// Deserialises a header from a 28-byte array.
    /// Returns `None` if the magic bytes do not match [`MAGIC`] or if the
    /// reserved flags field is non-zero.
    pub fn from_bytes(bytes: &[u8; HEADER_SIZE]) -> Option<Self> {
        if bytes[0..4] != MAGIC {
            return None;
        }
        let flags = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        if flags != 0 {
            return None;
        }
        Some(Self {
            version: u16::from_le_bytes([bytes[4], bytes[5]]),
            tier: bytes[6],
            codec: bytes[7],
            width: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            height: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            frame_count: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
            static_meta_len: u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
        })
    }

    /// Implicit raw pixel byte count per frame (used when codec is raw/uncompressed).
    ///
    /// Returns an error if size arithmetic overflows.
    pub fn raw_pixel_bytes_per_frame(&self) -> Result<usize, IoError> {
        let bytes_each: usize = if self.tier == TIER_DELIVERY { 4 } else { 8 };
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_mul(bytes_each))
            .ok_or(IoError::SizeOverflow {
                context: "raw pixel bytes per frame",
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let h = FileHeader {
            version: CURRENT_FORMAT_VERSION,
            tier: TIER_DELIVERY,
            codec: CODEC_AV1,
            width: 1920,
            height: 1080,
            frame_count: 24,
            static_meta_len: 512,
        };
        assert_eq!(h, FileHeader::from_bytes(&h.to_bytes()).unwrap());
    }

    #[test]
    fn test_header_rejects_bad_magic() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"NOPE");
        assert!(FileHeader::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_header_rejects_nonzero_flags() {
        let h = FileHeader {
            version: CONTAINER_VERSION_V1,
            tier: TIER_DELIVERY,
            codec: CODEC_AV1,
            width: 16,
            height: 16,
            frame_count: 1,
            static_meta_len: 0,
        };
        let mut bytes = h.to_bytes();
        bytes[20] = 1; // Set flags to non-zero.
        assert!(FileHeader::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_v1_pixel_bytes_delivery() {
        let h = FileHeader {
            version: CONTAINER_VERSION_V1,
            tier: TIER_DELIVERY,
            codec: CODEC_RAW,
            width: 4,
            height: 2,
            frame_count: 1,
            static_meta_len: 0,
        };
        // 4 × 2 × 3 channels × 4 bytes (f32) = 96
        assert_eq!(h.raw_pixel_bytes_per_frame().unwrap(), 96);
    }

    #[test]
    fn test_v1_pixel_bytes_mastering() {
        let h = FileHeader {
            version: CONTAINER_VERSION_V1,
            tier: TIER_MASTERING,
            codec: CODEC_RAW,
            width: 4,
            height: 2,
            frame_count: 1,
            static_meta_len: 0,
        };
        // 4 × 2 × 3 channels × 8 bytes (f64) = 192
        assert_eq!(h.raw_pixel_bytes_per_frame().unwrap(), 192);
    }

    #[test]
    fn test_raw_pixel_bytes_overflow_returns_error() {
        let h = FileHeader {
            version: CONTAINER_VERSION_V1,
            tier: TIER_MASTERING,
            codec: CODEC_RAW,
            width: u32::MAX,
            height: u32::MAX,
            frame_count: 1,
            static_meta_len: 0,
        };
        assert!(matches!(
            h.raw_pixel_bytes_per_frame(),
            Err(IoError::SizeOverflow {
                context: "raw pixel bytes per frame"
            })
        ));
    }

    #[test]
    fn test_codec_bytes_roundtrip() {
        for &codec in &[CODEC_RAW, CODEC_AV1] {
            let h = FileHeader {
                version: CURRENT_FORMAT_VERSION,
                tier: TIER_MASTERING,
                codec,
                width: 16,
                height: 16,
                frame_count: 1,
                static_meta_len: 256,
            };
            assert_eq!(FileHeader::from_bytes(&h.to_bytes()).unwrap().codec, codec);
        }
    }

    #[test]
    fn test_supported_container_versions() {
        assert!(is_supported_container_version(CONTAINER_VERSION_V1));
        assert!(is_supported_container_version(CONTAINER_VERSION_V2));
        assert!(!is_supported_container_version(0));
        assert!(!is_supported_container_version(3));
    }

    #[test]
    fn test_future_container_version_detection() {
        assert!(!is_future_container_version(CONTAINER_VERSION_V1));
        assert!(!is_future_container_version(CONTAINER_VERSION_V2));
        assert!(is_future_container_version(CONTAINER_VERSION_V2 + 1));
    }
}
