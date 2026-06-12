// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! QDRV binary container writer.
//!
//! Writes QDRV container files. Delivery-tier frames are AV1-encoded.
//! Mastering-tier frames are compressed with fpzip (default) or ZFP
//! reversible mode (optional `zfp` feature).

use std::io::Write;

use qdrv_codec::{Av1Config, MasteringCodec, av1_encode, mastering_compress};
use qdrv_core::pixel::{Pixel32, Pixel64};
use qdrv_meta::{
    DynamicMeta, StaticMeta,
    compatibility::{
        CompatibilityPolicy, METADATA_SCHEMA_V1, METADATA_SCHEMA_V2, validate_compatibility,
    },
};

use crate::{
    container::{
        CODEC_AV1, CONTAINER_VERSION_V1, CONTAINER_VERSION_V2, CURRENT_FORMAT_VERSION, FileHeader,
        TIER_DELIVERY, TIER_MASTERING, is_supported_container_version,
    },
    error::IoError,
};

fn usize_to_u32(value: usize, context: &'static str) -> Result<u32, IoError> {
    u32::try_from(value).map_err(|_| IoError::SizeOverflow { context })
}

/// A single frame for a QDRV delivery-tier file.
pub struct DeliveryFrame {
    /// Per-frame dynamic metadata.
    ///
    /// **Contract:** `dynamic_meta.frame_index` must equal the position of
    /// this frame in the `frames` slice passed to [`write_delivery_file`].
    /// The writer does not enforce this (it trusts caller-supplied
    /// metadata), but the reader does — files with out-of-sequence
    /// `frame_index` values are rejected at decode time with
    /// [`crate::IoError::InvalidMetadata`]. Built-in QDRV tooling sets the
    /// field correctly; library callers transcoding their own frames need
    /// to pass the position as `frame_index` to
    /// [`qdrv_encode::transcode_frame_with_options`] (or set the field
    /// manually before pushing into the writer's `frames` vector).
    pub dynamic_meta: DynamicMeta,
    /// Float32 PQ-encoded pixels. Length must equal `width × height`.
    pub pixels: Vec<Pixel32>,
}

/// A single frame for a QDRV mastering-tier file.
pub struct MasteringFrame {
    /// Per-frame dynamic metadata.
    ///
    /// **Contract:** see [`DeliveryFrame::dynamic_meta`] —
    /// `dynamic_meta.frame_index` must equal the frame's position in the
    /// `frames` slice passed to [`write_mastering_file`].
    pub dynamic_meta: DynamicMeta,
    /// Float64 linear light pixels in nits. Length must equal `width × height`.
    pub pixels: Vec<Pixel64>,
}

/// Writer options controlling QDRV container emission details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerWriteOptions {
    /// Container version to write to the output header.
    pub container_version: u16,
}

impl Default for ContainerWriteOptions {
    fn default() -> Self {
        Self {
            container_version: CURRENT_FORMAT_VERSION,
        }
    }
}

// ---------------------------------------------------------------------------
// Public write functions
// ---------------------------------------------------------------------------

/// Writes a complete QDRV delivery-tier (`.qdrv32`) file.
///
/// Each frame is encoded as a self-contained AV1 still-picture bitstream at
/// 12-bit YCbCr 4:4:4 with ITU-R Rec. 2100 HDR colour signalling.
///
/// # Errors
/// - [`IoError::Io`] — I/O write failed.
/// - [`IoError::PixelCountMismatch`] — frame pixel count ≠ `width × height`.
/// - [`IoError::MetaSerialisationFailed`] — metadata serialisation failed.
/// - [`IoError::Codec`] — AV1 encoding failed.
pub fn write_delivery_file<W: Write>(
    writer: &mut W,
    width: u32,
    height: u32,
    static_meta: &StaticMeta,
    frames: &[DeliveryFrame],
    av1_config: &Av1Config,
) -> Result<(), IoError> {
    write_delivery_file_with_options(
        writer,
        width,
        height,
        static_meta,
        frames,
        av1_config,
        ContainerWriteOptions::default(),
    )
}

/// Writes a complete QDRV delivery-tier file with explicit container options.
pub fn write_delivery_file_with_options<W: Write>(
    writer: &mut W,
    width: u32,
    height: u32,
    static_meta: &StaticMeta,
    frames: &[DeliveryFrame],
    av1_config: &Av1Config,
    options: ContainerWriteOptions,
) -> Result<(), IoError> {
    if width == 0 || height == 0 {
        return Err(IoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid frame dimensions: {width}x{height} (both must be > 0)"),
        )));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| {
            IoError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "width × height overflows usize",
            ))
        })?;
    for (i, f) in frames.iter().enumerate() {
        if f.pixels.len() != expected {
            return Err(IoError::PixelCountMismatch {
                frame: i,
                expected,
                actual: f.pixels.len(),
            });
        }
    }

    write_header(
        writer,
        TIER_DELIVERY,
        width,
        height,
        frames.len(),
        static_meta,
        options,
    )?;

    for (i, frame) in frames.iter().enumerate() {
        validate_compatibility(
            static_meta,
            &frame.dynamic_meta,
            CompatibilityPolicy::default(),
        )
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
        write_meta_block(writer, &frame.dynamic_meta)?;
        let av1 =
            av1_encode(&frame.pixels, width, height, av1_config).map_err(|e| IoError::Codec {
                frame: i,
                message: e.to_string(),
            })?;
        write_length_prefixed(writer, &av1, "delivery AV1 payload")?;
    }
    Ok(())
}

/// Writes a complete QDRV mastering-tier (`.qdrv64`) file.
///
/// Each frame is losslessly compressed with the specified [`MasteringCodec`].
/// The default is `MasteringCodec::Fpzip`, which is pure Rust and achieves
/// good compression ratios on smooth floating-point image data.
///
/// # Errors
/// - [`IoError::Io`] — I/O write failed.
/// - [`IoError::PixelCountMismatch`] — frame pixel count ≠ `width × height`.
/// - [`IoError::MetaSerialisationFailed`] — metadata serialisation failed.
/// - [`IoError::Codec`] — mastering compression failed.
pub fn write_mastering_file<W: Write>(
    writer: &mut W,
    width: u32,
    height: u32,
    static_meta: &StaticMeta,
    frames: &[MasteringFrame],
    mastering_codec: MasteringCodec,
) -> Result<(), IoError> {
    write_mastering_file_with_options(
        writer,
        width,
        height,
        static_meta,
        frames,
        mastering_codec,
        ContainerWriteOptions::default(),
    )
}

/// Writes a complete QDRV mastering-tier file with explicit container options.
pub fn write_mastering_file_with_options<W: Write>(
    writer: &mut W,
    width: u32,
    height: u32,
    static_meta: &StaticMeta,
    frames: &[MasteringFrame],
    mastering_codec: MasteringCodec,
    options: ContainerWriteOptions,
) -> Result<(), IoError> {
    if width == 0 || height == 0 {
        return Err(IoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid frame dimensions: {width}x{height} (both must be > 0)"),
        )));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| {
            IoError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "width × height overflows usize",
            ))
        })?;
    for (i, f) in frames.iter().enumerate() {
        if f.pixels.len() != expected {
            return Err(IoError::PixelCountMismatch {
                frame: i,
                expected,
                actual: f.pixels.len(),
            });
        }
    }

    write_header(
        writer,
        TIER_MASTERING,
        width,
        height,
        frames.len(),
        static_meta,
        options,
    )?;

    for (i, frame) in frames.iter().enumerate() {
        validate_compatibility(
            static_meta,
            &frame.dynamic_meta,
            CompatibilityPolicy::default(),
        )
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
        write_meta_block(writer, &frame.dynamic_meta)?;
        let compressed = mastering_compress(&frame.pixels, width, height, mastering_codec)
            .map_err(|e| IoError::Codec {
                frame: i,
                message: e.to_string(),
            })?;
        write_length_prefixed(writer, &compressed, "mastering payload")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn write_header<W: Write>(
    writer: &mut W,
    tier: u8,
    width: u32,
    height: u32,
    frame_count: usize,
    static_meta: &StaticMeta,
    options: ContainerWriteOptions,
) -> Result<(), IoError> {
    if !is_supported_container_version(options.container_version) {
        return Err(IoError::UnsupportedWriteVersion(options.container_version));
    }
    static_meta
        .validate()
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
    enforce_container_metadata_schema(
        options.container_version,
        static_meta.metadata_schema_version,
    )?;

    let static_json = qdrv_meta::to_json(static_meta)
        .map_err(|e| IoError::MetaSerialisationFailed(e.to_string()))?;
    let static_bytes = static_json.as_bytes();
    let frame_count = usize_to_u32(frame_count, "frame count in header")?;
    let static_meta_len = usize_to_u32(static_bytes.len(), "static metadata length in header")?;

    let header = FileHeader {
        version: options.container_version,
        tier,
        codec: CODEC_AV1,
        width,
        height,
        frame_count,
        static_meta_len,
    };
    writer.write_all(&header.to_bytes())?;
    writer.write_all(static_bytes)?;
    Ok(())
}

fn enforce_container_metadata_schema(
    container_version: u16,
    metadata_schema_version: u16,
) -> Result<(), IoError> {
    match container_version {
        CONTAINER_VERSION_V1 => {
            if metadata_schema_version != METADATA_SCHEMA_V1 {
                return Err(IoError::InvalidMetadata(format!(
                    "container version {CONTAINER_VERSION_V1} requires metadata schema \
                     version {METADATA_SCHEMA_V1}, got {metadata_schema_version}"
                )));
            }
            Ok(())
        }
        CONTAINER_VERSION_V2 => {
            if metadata_schema_version == METADATA_SCHEMA_V1
                || metadata_schema_version == METADATA_SCHEMA_V2
            {
                Ok(())
            } else {
                Err(IoError::InvalidMetadata(format!(
                    "container version {CONTAINER_VERSION_V2} does not support metadata schema \
                     version {metadata_schema_version}"
                )))
            }
        }
        _ => Err(IoError::UnsupportedWriteVersion(container_version)),
    }
}

fn write_meta_block<W: Write>(
    writer: &mut W,
    meta: &qdrv_meta::DynamicMeta,
) -> Result<(), IoError> {
    meta.validate()
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
    let json =
        qdrv_meta::to_json(meta).map_err(|e| IoError::MetaSerialisationFailed(e.to_string()))?;
    write_length_prefixed(writer, json.as_bytes(), "dynamic metadata")
}

fn write_length_prefixed<W: Write>(
    writer: &mut W,
    data: &[u8],
    context: &'static str,
) -> Result<(), IoError> {
    let len: u32 = data.len().try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{context} length {} exceeds u32::MAX", data.len()),
        )
    })?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{CONTAINER_VERSION_V1, CONTAINER_VERSION_V2, HEADER_SIZE};
    use qdrv_meta::DynamicMeta;
    use std::io::Cursor;

    fn delivery_frame() -> DeliveryFrame {
        DeliveryFrame {
            dynamic_meta: DynamicMeta::new(0, 1000.0, 300.0),
            pixels: vec![Pixel32::new_unchecked(0.1, 0.2, 0.3)],
        }
    }

    fn default_av1_config() -> Av1Config {
        Av1Config {
            speed: 10,
            quantizer: 0,
            lossless: true,
            threads: 1,
            ..Default::default()
        }
    }

    #[test]
    fn write_defaults_to_container_v2() {
        let static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        let frame = delivery_frame();
        let mut out = Cursor::new(Vec::<u8>::new());

        write_delivery_file(
            &mut out,
            1,
            1,
            &static_meta,
            &[frame],
            &default_av1_config(),
        )
        .unwrap();
        let bytes = out.into_inner();
        let mut header = [0u8; HEADER_SIZE];
        header.copy_from_slice(&bytes[..HEADER_SIZE]);
        let parsed = FileHeader::from_bytes(&header).unwrap();
        assert_eq!(parsed.version, CONTAINER_VERSION_V2);
    }

    #[test]
    fn write_can_emit_container_v1_explicitly() {
        let static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        let frame = delivery_frame();
        let mut out = Cursor::new(Vec::<u8>::new());

        write_delivery_file_with_options(
            &mut out,
            1,
            1,
            &static_meta,
            &[frame],
            &default_av1_config(),
            ContainerWriteOptions {
                container_version: CONTAINER_VERSION_V1,
            },
        )
        .unwrap();

        let bytes = out.into_inner();
        let mut header = [0u8; HEADER_SIZE];
        header.copy_from_slice(&bytes[..HEADER_SIZE]);
        let parsed = FileHeader::from_bytes(&header).unwrap();
        assert_eq!(parsed.version, CONTAINER_VERSION_V1);
    }

    #[test]
    fn write_rejects_v1_with_v2_metadata_schema() {
        let mut static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        static_meta.metadata_schema_version = METADATA_SCHEMA_V2;
        let frame = delivery_frame();
        let mut out = Cursor::new(Vec::<u8>::new());

        let err = write_delivery_file_with_options(
            &mut out,
            1,
            1,
            &static_meta,
            &[frame],
            &default_av1_config(),
            ContainerWriteOptions {
                container_version: CONTAINER_VERSION_V1,
            },
        )
        .unwrap_err();

        assert!(matches!(err, IoError::InvalidMetadata(_)));
    }
}
