// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! QDRV binary container reader.
//!
//! Accepts container versions `1` and `2`.
//! Supports both codec modes: raw uncompressed (codec byte 0, for testing)
//! and the production codec (codec byte 1: AV1 delivery, fpzip/ZFP mastering).
//! All files share the same 28-byte header layout.

use std::io::Read;

use qdrv_codec::{av1_decode, mastering_decompress};
use qdrv_core::pixel::{Pixel32, Pixel64};
use qdrv_meta::{
    DynamicMeta, StaticMeta,
    compatibility::{
        CompatibilityPolicy, METADATA_SCHEMA_V1, METADATA_SCHEMA_V2, validate_compatibility,
    },
};

use crate::{
    container::{
        CODEC_AV1, CODEC_RAW, CONTAINER_VERSION_V1, CONTAINER_VERSION_V2, FileHeader, HEADER_SIZE,
        TIER_DELIVERY, TIER_MASTERING, is_future_container_version, is_supported_container_version,
    },
    error::IoError,
};

/// Maximum accepted JSON block size for static/dynamic metadata.
const MAX_JSON_BLOCK_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on decoded frame area (`width × height`).
const MAX_FRAME_PIXELS: usize = 16 * 1024 * 1024;
/// Upper bound on accepted frame payload allocations.
const MAX_FRAME_PAYLOAD_BYTES: usize = 512 * 1024 * 1024;
/// Upper bound on declared frame count from untrusted headers.
const MAX_FRAME_COUNT: usize = 100_000;
/// Floor for compressed frame budget to avoid rejecting tiny images.
const MIN_COMPRESSED_FRAME_BUDGET: usize = 256 * 1024;

/// The decoded pixel buffer for a single QDRV frame.
#[derive(Debug, Clone)]
pub enum PixelBuffer {
    /// Delivery-tier pixels: Float32 SMPTE ST 2084 PQ-encoded, Rec. 2100.
    Delivery(Vec<Pixel32>),
    /// Mastering-tier pixels: Float64 linear light in nits, Rec. 2100.
    Mastering(Vec<Pixel64>),
}

impl PixelBuffer {
    /// Returns the number of pixels in this buffer.
    pub fn len(&self) -> usize {
        match self {
            Self::Delivery(v) => v.len(),
            Self::Mastering(v) => v.len(),
        }
    }

    /// Returns `true` if the buffer contains no pixels.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the delivery-tier pixels, or `None` for a mastering buffer.
    pub fn as_delivery(&self) -> Option<&[Pixel32]> {
        if let Self::Delivery(v) = self {
            Some(v)
        } else {
            None
        }
    }

    /// Returns the mastering-tier pixels, or `None` for a delivery buffer.
    pub fn as_mastering(&self) -> Option<&[Pixel64]> {
        if let Self::Mastering(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

/// A single decoded QDRV frame.
#[derive(Debug, Clone)]
pub struct QdrvFrame {
    /// Per-frame dynamic metadata containing scene statistics and tone curve.
    pub dynamic_meta: DynamicMeta,
    /// Decoded pixel buffer.
    pub pixels: PixelBuffer,
}

/// A fully decoded QDRV file.
#[derive(Debug, Clone)]
pub struct QdrvFile {
    /// Parsed file header.
    pub header: FileHeader,
    /// Static stream metadata from the header block.
    pub static_meta: StaticMeta,
    /// All frames in presentation order.
    pub frames: Vec<QdrvFrame>,
}

impl QdrvFile {
    /// Frame width in pixels.
    pub fn width(&self) -> u32 {
        self.header.width
    }
    /// Frame height in pixels.
    pub fn height(&self) -> u32 {
        self.header.height
    }
    /// Number of frames.
    pub fn frame_count(&self) -> u32 {
        self.header.frame_count
    }
    /// `true` if this is a delivery-tier (Float32 PQ) file.
    pub fn is_delivery(&self) -> bool {
        self.header.tier == TIER_DELIVERY
    }
    /// `true` if this is a mastering-tier (Float64 linear) file.
    pub fn is_mastering(&self) -> bool {
        self.header.tier == TIER_MASTERING
    }
}

/// Streaming QDRV reader that decodes one frame at a time.
pub struct QdrvStreamReader<R: Read> {
    reader: R,
    header: FileHeader,
    static_meta: StaticMeta,
    expected_pixels: usize,
    frames_read: usize,
}

impl<R: Read> QdrvStreamReader<R> {
    /// Opens a streaming reader and parses header/static metadata eagerly.
    pub fn new(mut reader: R) -> Result<Self, IoError> {
        let (header, static_meta, expected_pixels) = read_header_and_static_meta(&mut reader)?;
        Ok(Self {
            reader,
            header,
            static_meta,
            expected_pixels,
            frames_read: 0,
        })
    }

    /// Returns the parsed file header.
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// Returns the parsed static metadata block.
    pub fn static_meta(&self) -> &StaticMeta {
        &self.static_meta
    }

    /// Total number of frames declared by the header.
    pub fn frame_count(&self) -> u32 {
        self.header.frame_count
    }

    /// Number of frames already yielded by this stream.
    pub fn frames_read(&self) -> usize {
        self.frames_read
    }

    /// Reads and decodes the next frame, or `Ok(None)` at end of stream.
    pub fn next_frame(&mut self) -> Result<Option<QdrvFrame>, IoError> {
        let frame_count = self.header.frame_count as usize;
        if self.frames_read >= frame_count {
            return Ok(None);
        }
        let frame_idx = self.frames_read;
        let frame = read_one_frame(
            &mut self.reader,
            &self.header,
            &self.static_meta,
            self.expected_pixels,
            frame_idx,
        )?;
        self.frames_read += 1;
        Ok(Some(frame))
    }
}

impl<R: Read> Iterator for QdrvStreamReader<R> {
    type Item = Result<QdrvFrame, IoError>;

    fn next(&mut self) -> Option<Self::Item> {
        match Self::next_frame(self) {
            Ok(Some(frame)) => Some(Ok(frame)),
            Ok(None) => None,
            Err(err) => {
                self.frames_read = self.header.frame_count as usize;
                Some(Err(err))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public read function
// ---------------------------------------------------------------------------

/// Reads a complete QDRV file from the given reader.
///
/// Validates the header, reads the static metadata, and decodes all per-frame
/// blocks. Supports both codec modes (raw uncompressed and AV1/fpzip/ZFP).
///
/// # Errors
/// - [`IoError::Io`] — an I/O read failed.
/// - [`IoError::InvalidMagic`] — wrong magic bytes.
/// - [`IoError::UnsupportedVersion`] — version is unknown/deprecated.
/// - [`IoError::FutureVersion`] — version is newer than this implementation.
/// - [`IoError::InvalidTier`] — tier byte is not `0` or `1`.
/// - [`IoError::InvalidCodec`] — codec byte is not `0` or `1`.
/// - [`IoError::InvalidMetadata`] — JSON parse failed.
/// - [`IoError::TruncatedPixelData`] — pixel data shorter than expected.
/// - [`IoError::SizeLimitExceeded`] — declared untrusted sizes exceed hard bounds.
/// - [`IoError::SizeOverflow`] — size arithmetic overflowed.
/// - [`IoError::AllocationFailed`] — bounded allocation could not be reserved.
/// - [`IoError::Codec`] — AV1 or fpzip/ZFP decode failed.
pub fn read_file<R: Read>(reader: &mut R) -> Result<QdrvFile, IoError> {
    let mut stream = QdrvStreamReader::new(reader)?;
    let header = stream.header.clone();
    let static_meta = stream.static_meta.clone();
    // `frame_count` is already bounded against `MAX_FRAME_COUNT` inside
    // `QdrvStreamReader::new`, so this allocation cannot exceed
    // ~100K * sizeof(QdrvFrame). We still use `try_reserve_exact` instead
    // of `with_capacity` so a hostile header that survived bounds checks
    // but met a runtime memory ceiling reports `AllocationFailed`
    // gracefully rather than aborting the process via OOM. This matches
    // the per-frame decode paths and addresses the N-3 follow-up.
    let mut frames: Vec<QdrvFrame> = Vec::new();
    let frame_count = header.frame_count as usize;
    frames
        .try_reserve_exact(frame_count)
        .map_err(|_| IoError::AllocationFailed {
            context: "frames vector",
            requested: frame_count.saturating_mul(std::mem::size_of::<QdrvFrame>()),
        })?;
    while let Some(frame) = stream.next_frame()? {
        frames.push(frame);
    }
    Ok(QdrvFile {
        header,
        static_meta,
        frames,
    })
}

fn read_header_and_static_meta<R: Read>(
    reader: &mut R,
) -> Result<(FileHeader, StaticMeta, usize), IoError> {
    // Read and validate the file header.
    let mut header_buf = [0u8; HEADER_SIZE];
    reader.read_exact(&mut header_buf)?;

    let header = FileHeader::from_bytes(&header_buf).ok_or(IoError::InvalidMagic)?;

    if is_future_container_version(header.version) {
        return Err(IoError::FutureVersion(header.version));
    }
    if !is_supported_container_version(header.version) {
        return Err(IoError::UnsupportedVersion(header.version));
    }
    if header.tier != TIER_DELIVERY && header.tier != TIER_MASTERING {
        return Err(IoError::InvalidTier(header.tier));
    }
    if header.codec != CODEC_RAW && header.codec != CODEC_AV1 {
        return Err(IoError::InvalidCodec(header.codec));
    }
    if header.width == 0 || header.height == 0 {
        return Err(IoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid frame dimensions in header: {}x{} (both must be > 0)",
                header.width, header.height
            ),
        )));
    }
    let frame_count = header.frame_count as usize;
    ensure_within_limit("frame count", frame_count, MAX_FRAME_COUNT)?;

    // Read the static metadata JSON block.
    let static_meta: StaticMeta = read_json_block(
        reader,
        header.static_meta_len as usize,
        "metadata JSON block",
    )?;
    static_meta
        .validate()
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
    ensure_metadata_schema_supported_for_container(
        header.version,
        static_meta.metadata_schema_version,
    )?;

    let w = header.width as usize;
    let h = header.height as usize;
    let expected_pixels = checked_mul_usize(w, h, "frame pixel count (width × height)")?;
    ensure_within_limit("frame area pixels", expected_pixels, MAX_FRAME_PIXELS)?;
    Ok((header, static_meta, expected_pixels))
}

fn read_one_frame<R: Read>(
    reader: &mut R,
    header: &FileHeader,
    static_meta: &StaticMeta,
    expected_pixels: usize,
    frame_idx: usize,
) -> Result<QdrvFrame, IoError> {
    let w = header.width as usize;
    let h = header.height as usize;

    // Dynamic metadata length + JSON.
    let dyn_len = read_u32_le(reader, frame_idx)? as usize;
    let dynamic_meta: DynamicMeta = read_json_block(reader, dyn_len, "metadata JSON block")?;
    dynamic_meta
        .validate()
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
    ensure_metadata_schema_supported_for_container(
        header.version,
        dynamic_meta.metadata_schema_version,
    )?;
    validate_compatibility(static_meta, &dynamic_meta, CompatibilityPolicy::default())
        .map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
    // P3-3 sequence guard: every QDRV writer in this workspace sets
    // `DynamicMeta::frame_index` to the frame's position in the stream
    // (the existing `test_multi_frame_order_preserved` integration test
    // asserts this contract). A mismatch here means the file is either
    // hand-crafted or corrupted — surface that to the operator instead
    // of letting nonsense indices flow into `qdrv inspect` /
    // `qdrv hdr10plus` output. Implicitly also bounds `frame_index`
    // to `MAX_FRAME_COUNT - 1`.
    if dynamic_meta.frame_index != frame_idx as u64 {
        return Err(IoError::InvalidMetadata(format!(
            "frame {frame_idx}: dynamic_meta.frame_index = {} does not match stream position",
            dynamic_meta.frame_index
        )));
    }

    // Pixel data — format depends on codec byte.
    let pixels = if header.codec == CODEC_RAW {
        // Codec 0: raw uncompressed bytes, length is implicit from dimensions.
        let byte_count = header.raw_pixel_bytes_per_frame()?;
        ensure_within_limit("raw pixel payload", byte_count, MAX_FRAME_PAYLOAD_BYTES)?;
        read_v1_pixels(reader, frame_idx, header.tier, w, h, byte_count)?
    } else {
        // Codec 1: compressed bytes with u32 length prefix.
        let pixel_len = read_u32_le(reader, frame_idx)? as usize;
        let max_pixel_len = compressed_frame_budget(expected_pixels, header.tier)?;
        ensure_within_limit("compressed pixel payload", pixel_len, max_pixel_len)?;
        let pixel_data =
            read_exact_bytes(reader, frame_idx, pixel_len, "compressed pixel payload")?;
        decode_codec_pixels(&pixel_data, frame_idx, header.tier, w, h, expected_pixels)?
    };

    Ok(QdrvFrame {
        dynamic_meta,
        pixels,
    })
}

fn checked_mul_usize(a: usize, b: usize, context: &'static str) -> Result<usize, IoError> {
    a.checked_mul(b).ok_or(IoError::SizeOverflow { context })
}

fn read_f64_chunk(data: &[u8], start: usize, frame_idx: usize) -> Result<f64, IoError> {
    let end = start.checked_add(8).ok_or(IoError::SizeOverflow {
        context: "f64 byte range",
    })?;
    let bytes = data.get(start..end).ok_or_else(|| IoError::Codec {
        frame: frame_idx,
        message: "raw mastering pixel chunk is truncated".to_string(),
    })?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    Ok(f64::from_le_bytes(raw))
}

fn ensure_within_limit(
    context: &'static str,
    declared: usize,
    maximum: usize,
) -> Result<(), IoError> {
    if declared > maximum {
        return Err(IoError::SizeLimitExceeded {
            context,
            declared,
            maximum,
        });
    }
    Ok(())
}

fn compressed_frame_budget(expected_pixels: usize, tier: u8) -> Result<usize, IoError> {
    let bytes_per_channel = if tier == TIER_DELIVERY {
        4usize
    } else {
        8usize
    };
    let channels = 3usize;
    let uncompressed = checked_mul_usize(
        checked_mul_usize(expected_pixels, channels, "frame channel sample count")?,
        bytes_per_channel,
        "uncompressed frame byte size",
    )?;
    // Allow expansion over raw size for small/high-entropy payloads while still
    // constraining allocations from untrusted length prefixes.
    let expanded = checked_mul_usize(uncompressed, 8, "compressed frame budget")?;
    Ok(expanded.clamp(MIN_COMPRESSED_FRAME_BUDGET, MAX_FRAME_PAYLOAD_BYTES))
}

fn ensure_metadata_schema_supported_for_container(
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
        _ => Err(IoError::UnsupportedVersion(container_version)),
    }
}

// ---------------------------------------------------------------------------
// Private helpers — version 1
// ---------------------------------------------------------------------------

/// Reads raw (uncompressed) pixel data from a v1 frame block.
fn read_v1_pixels<R: Read>(
    reader: &mut R,
    frame_idx: usize,
    tier: u8,
    w: usize,
    h: usize,
    byte_count: usize,
) -> Result<PixelBuffer, IoError> {
    let raw = read_exact_bytes(reader, frame_idx, byte_count, "raw pixel payload")?;

    let pixel_count = checked_mul_usize(w, h, "frame pixel count (width × height)")?;
    let bpp = if tier == TIER_DELIVERY {
        12usize
    } else {
        24usize
    };
    let expected_raw = checked_mul_usize(pixel_count, bpp, "raw pixel byte count")?;
    if byte_count != expected_raw {
        return Err(IoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "raw pixel block in frame {frame_idx}: {byte_count} bytes; expected {expected_raw} \
                 for tier {tier} and dimensions {w}×{h}",
            ),
        )));
    }

    if tier == TIER_DELIVERY {
        // Float32 LE, 3 channels, row-major RGB.
        // Use try_reserve_exact so an adversarially-large but in-budget
        // frame returns AllocationFailed instead of panicking on OOM.
        let mut pixels: Vec<Pixel32> = Vec::new();
        pixels
            .try_reserve_exact(pixel_count)
            .map_err(|_| IoError::AllocationFailed {
                context: "delivery pixel buffer",
                requested: pixel_count.saturating_mul(std::mem::size_of::<Pixel32>()),
            })?;
        for chunk in raw.chunks_exact(12) {
            let r = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let g = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let b = f32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
            let px = Pixel32::new(r, g, b).map_err(|e| IoError::Codec {
                frame: frame_idx,
                message: format!("raw delivery pixel contains non-finite channel: {e}"),
            })?;
            pixels.push(px);
        }
        Ok(PixelBuffer::Delivery(pixels))
    } else {
        // Float64 LE, 3 channels, row-major RGB.
        let mut pixels: Vec<Pixel64> = Vec::new();
        pixels
            .try_reserve_exact(pixel_count)
            .map_err(|_| IoError::AllocationFailed {
                context: "mastering pixel buffer",
                requested: pixel_count.saturating_mul(std::mem::size_of::<Pixel64>()),
            })?;
        for chunk in raw.chunks_exact(24) {
            let r = read_f64_chunk(chunk, 0, frame_idx)?;
            let g = read_f64_chunk(chunk, 8, frame_idx)?;
            let b = read_f64_chunk(chunk, 16, frame_idx)?;
            let px = Pixel64::new(r, g, b).map_err(|e| IoError::Codec {
                frame: frame_idx,
                message: format!("raw mastering pixel contains non-finite channel: {e}"),
            })?;
            pixels.push(px);
        }
        Ok(PixelBuffer::Mastering(pixels))
    }
}

// ---------------------------------------------------------------------------
// Private helpers — production codec (codec=1)
// ---------------------------------------------------------------------------

/// Decodes codec-compressed pixel data from a codec=1 frame block.
fn decode_codec_pixels(
    data: &[u8],
    frame_idx: usize,
    tier: u8,
    w: usize,
    h: usize,
    expected_pixels: usize,
) -> Result<PixelBuffer, IoError> {
    if tier == TIER_DELIVERY {
        let pixels = av1_decode(data, w as u32, h as u32).map_err(|e| IoError::Codec {
            frame: frame_idx,
            message: e.to_string(),
        })?;
        if pixels.len() != expected_pixels {
            return Err(IoError::Codec {
                frame: frame_idx,
                message: format!(
                    "decoded delivery pixel count mismatch: expected {expected_pixels}, got {}",
                    pixels.len()
                ),
            });
        }
        Ok(PixelBuffer::Delivery(pixels))
    } else {
        let pixels = mastering_decompress(data, expected_pixels).map_err(|e| IoError::Codec {
            frame: frame_idx,
            message: e.to_string(),
        })?;
        if pixels.len() != expected_pixels {
            return Err(IoError::Codec {
                frame: frame_idx,
                message: format!(
                    "decoded mastering pixel count mismatch: expected {expected_pixels}, got {}",
                    pixels.len()
                ),
            });
        }
        Ok(PixelBuffer::Mastering(pixels))
    }
}

// ---------------------------------------------------------------------------
// Low-level I/O helpers
// ---------------------------------------------------------------------------

/// Reads a `u32` little-endian value as a length prefix.
fn read_u32_le<R: Read>(reader: &mut R, frame_idx: usize) -> Result<u32, IoError> {
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|e| IoError::TruncatedPixelData {
            frame: frame_idx,
            expected: 4,
            source: e,
        })?;
    Ok(u32::from_le_bytes(buf))
}

/// Reads exactly `len` bytes from the reader into a `Vec<u8>`.
fn read_exact_bytes<R: Read>(
    reader: &mut R,
    frame_idx: usize,
    len: usize,
    context: &'static str,
) -> Result<Vec<u8>, IoError> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| IoError::AllocationFailed {
            context,
            requested: len,
        })?;
    buf.resize(len, 0);
    reader
        .read_exact(&mut buf)
        .map_err(|e| IoError::TruncatedPixelData {
            frame: frame_idx,
            expected: len,
            source: e,
        })?;
    Ok(buf)
}

/// Reads `len` bytes and deserialises them as a UTF-8 JSON block into `T`.
fn read_json_block<R: Read, T: serde::de::DeserializeOwned>(
    reader: &mut R,
    len: usize,
    context: &'static str,
) -> Result<T, IoError> {
    ensure_within_limit(context, len, MAX_JSON_BLOCK_BYTES)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| IoError::AllocationFailed {
            context,
            requested: len,
        })?;
    buf.resize(len, 0);
    reader.read_exact(&mut buf)?;
    let json = std::str::from_utf8(&buf).map_err(|e| IoError::InvalidMetadata(e.to_string()))?;
    qdrv_meta::from_json(json).map_err(|e| IoError::InvalidMetadata(e.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{
        CODEC_AV1, CODEC_RAW, CONTAINER_VERSION_V1, CONTAINER_VERSION_V2, CURRENT_FORMAT_VERSION,
        FileHeader, HEADER_SIZE, TIER_DELIVERY,
    };
    use crate::writer::{
        ContainerWriteOptions, DeliveryFrame, MasteringFrame, write_delivery_file,
        write_delivery_file_with_options, write_mastering_file,
    };
    use qdrv_codec::{Av1Config, ChromaSampling420, MasteringCodec};
    use qdrv_meta::{DynamicMeta, StaticMeta};
    use std::io::Cursor;

    fn delivery_frame(idx: u64, w: u32, h: u32, r: f32, g: f32, b: f32) -> DeliveryFrame {
        let pixels = vec![Pixel32::new_unchecked(r, g, b); (w as usize).saturating_mul(h as usize)];
        DeliveryFrame {
            dynamic_meta: DynamicMeta::new(idx, 500.0, 100.0),
            pixels,
        }
    }

    fn mastering_frame(idx: u64, w: u32, h: u32, r: f64, g: f64, b: f64) -> MasteringFrame {
        let pixels = vec![Pixel64::new_unchecked(r, g, b); (w as usize).saturating_mul(h as usize)];
        MasteringFrame {
            dynamic_meta: DynamicMeta::new(idx, 1000.0, 200.0),
            pixels,
        }
    }

    #[test]
    fn test_delivery_roundtrip() {
        // Writing and reading back a delivery file must recover metadata
        // and pixel values within Float32 precision loss of the 12-bit AV1
        // quantisation step.
        let meta = StaticMeta::default_delivery(800.0, 300.0);
        let w = 8u32;
        let h = 4u32;
        // Use a PQ value near 0.5 (roughly 500 nits) for a non-trivial test.
        let frames = vec![
            delivery_frame(0, w, h, 0.45, 0.50, 0.55),
            delivery_frame(1, w, h, 0.30, 0.35, 0.40),
        ];
        let av1_cfg = Av1Config {
            speed: 10,
            quantizer: 0,
            lossless: true,
            threads: 1,
            chroma: ChromaSampling420::Cs444,
        };

        let mut buf = Cursor::new(Vec::<u8>::new());
        write_delivery_file(&mut buf, w, h, &meta, &frames, &av1_cfg).unwrap();

        buf.set_position(0);
        let qdrv = read_file(&mut buf).unwrap();

        assert_eq!(qdrv.header.version, CURRENT_FORMAT_VERSION);
        assert!(qdrv.is_delivery());
        assert_eq!(qdrv.frames.len(), 2);
        assert_eq!(qdrv.static_meta, meta);

        // With lossless AV1 (quantizer=0), the 12-bit representation must
        // be preserved exactly. The Float32 → 12-bit quantisation introduces
        // up to 1/4095 ≈ 0.000244 of error per channel.
        let pixels = qdrv.frames[0].pixels.as_delivery().unwrap();
        let tolerance = 1.0 / 4095.0 + f32::EPSILON * 10.0;
        assert!(
            (pixels[0].r - 0.45).abs() <= tolerance,
            "R channel delta {} exceeds tolerance {tolerance}",
            (pixels[0].r - 0.45).abs()
        );
        assert!(
            (pixels[0].g - 0.50).abs() <= tolerance,
            "G channel delta {} exceeds tolerance {tolerance}",
            (pixels[0].g - 0.50).abs()
        );
        assert!(
            (pixels[0].b - 0.55).abs() <= tolerance,
            "B channel delta {} exceeds tolerance {tolerance}",
            (pixels[0].b - 0.55).abs()
        );
    }

    #[test]
    fn test_mastering_roundtrip() {
        // fpzip is lossless, so the mastering tier must recover Float64 values exactly.
        let meta = StaticMeta::default_mastering();
        let w = 4u32;
        let h = 4u32;
        let frames = vec![mastering_frame(0, w, h, 5000.0, 2500.0, 1000.0)];

        let mut buf = Cursor::new(Vec::<u8>::new());
        write_mastering_file(&mut buf, w, h, &meta, &frames, MasteringCodec::Fpzip).unwrap();

        buf.set_position(0);
        let qdrv = read_file(&mut buf).unwrap();

        assert!(qdrv.is_mastering());
        let pixels = qdrv.frames[0].pixels.as_mastering().unwrap();
        assert_eq!(
            pixels[0].r, 5000.0,
            "R channel not exact after fpzip roundtrip"
        );
        assert_eq!(
            pixels[0].g, 2500.0,
            "G channel not exact after fpzip roundtrip"
        );
        assert_eq!(
            pixels[0].b, 1000.0,
            "B channel not exact after fpzip roundtrip"
        );
    }

    #[test]
    fn test_above_pq_ceiling_survives_mastering_roundtrip() {
        // A luminance value above 10 000 nits (the ST 2084 PQ ceiling) must
        // be preserved exactly in the mastering tier — this is a core QDRV
        // requirement that no existing HDR format satisfies.
        let meta = StaticMeta::default_mastering();
        let frames = vec![mastering_frame(0, 2, 2, 50_000.0, 25_000.0, 12_500.0)];

        let mut buf = Cursor::new(Vec::<u8>::new());
        write_mastering_file(&mut buf, 2, 2, &meta, &frames, MasteringCodec::Fpzip).unwrap();

        buf.set_position(0);
        let qdrv = read_file(&mut buf).unwrap();

        let pixels = qdrv.frames[0].pixels.as_mastering().unwrap();
        assert_eq!(pixels[0].r, 50_000.0);
        assert_eq!(pixels[0].g, 25_000.0);
        assert_eq!(pixels[0].b, 12_500.0);
    }

    #[test]
    fn test_multi_frame_order_preserved() {
        // Frame indices in the dynamic metadata must be preserved in order.
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let frames: Vec<DeliveryFrame> = (0..4)
            .map(|i| delivery_frame(i as u64, 4, 4, 0.1 * i as f32, 0.0, 0.0))
            .collect();
        let av1_cfg = Av1Config {
            speed: 10,
            quantizer: 60,
            lossless: false,
            threads: 1,
            chroma: ChromaSampling420::Cs444,
        };

        let mut buf = Cursor::new(Vec::<u8>::new());
        write_delivery_file(&mut buf, 4, 4, &meta, &frames, &av1_cfg).unwrap();

        buf.set_position(0);
        let qdrv = read_file(&mut buf).unwrap();

        for (i, frame) in qdrv.frames.iter().enumerate() {
            assert_eq!(frame.dynamic_meta.frame_index, i as u64);
        }
    }

    #[test]
    fn test_stream_reader_iterates_frames_in_order() {
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let frames = vec![
            delivery_frame(0, 4, 2, 0.10, 0.20, 0.30),
            delivery_frame(1, 4, 2, 0.40, 0.50, 0.60),
        ];
        let av1_cfg = Av1Config {
            speed: 10,
            quantizer: 0,
            lossless: true,
            threads: 1,
            chroma: ChromaSampling420::Cs444,
        };

        let mut buf = Cursor::new(Vec::<u8>::new());
        write_delivery_file(&mut buf, 4, 2, &meta, &frames, &av1_cfg).unwrap();
        buf.set_position(0);

        let mut stream = QdrvStreamReader::new(&mut buf).unwrap();
        assert_eq!(stream.frame_count(), 2);
        assert_eq!(stream.header().width, 4);
        assert_eq!(stream.header().height, 2);
        assert_eq!(stream.static_meta(), &meta);
        assert_eq!(stream.frames_read(), 0);

        let f0 = stream.next_frame().unwrap().expect("frame 0");
        let f1 = stream.next_frame().unwrap().expect("frame 1");
        assert!(stream.next_frame().unwrap().is_none());
        assert_eq!(f0.dynamic_meta.frame_index, 0);
        assert_eq!(f1.dynamic_meta.frame_index, 1);
        assert_eq!(stream.frames_read(), 2);
    }

    #[test]
    fn test_read_rejects_invalid_magic() {
        let mut buf = Cursor::new(vec![0u8; 64]);
        assert!(matches!(read_file(&mut buf), Err(IoError::InvalidMagic)));
    }

    #[test]
    fn test_write_rejects_pixel_count_mismatch() {
        let meta = StaticMeta::default_delivery(1000.0, 400.0);
        let frame = DeliveryFrame {
            dynamic_meta: DynamicMeta::new(0, 500.0, 100.0),
            pixels: vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 3],
        };
        let av1_cfg = Av1Config::default();
        let mut buf = Cursor::new(Vec::<u8>::new());
        let result = write_delivery_file(&mut buf, 4, 4, &meta, &[frame], &av1_cfg);
        assert!(matches!(result, Err(IoError::PixelCountMismatch { .. })));
    }

    #[test]
    fn test_read_rejects_oversized_static_metadata_length() {
        let header = FileHeader {
            version: CURRENT_FORMAT_VERSION,
            tier: TIER_DELIVERY,
            codec: CODEC_AV1,
            width: 1,
            height: 1,
            frame_count: 0,
            static_meta_len: (MAX_JSON_BLOCK_BYTES as u32) + 1,
        };
        let mut buf = Cursor::new(header.to_bytes().to_vec());
        let err = read_file(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            IoError::SizeLimitExceeded {
                context: "metadata JSON block",
                ..
            }
        ));
    }

    #[test]
    fn test_read_rejects_oversized_dynamic_metadata_length() {
        let static_meta = qdrv_meta::to_json(&StaticMeta::default_delivery(1000.0, 400.0)).unwrap();
        let header = FileHeader {
            version: CURRENT_FORMAT_VERSION,
            tier: TIER_DELIVERY,
            codec: CODEC_AV1,
            width: 1,
            height: 1,
            frame_count: 1,
            static_meta_len: static_meta.len() as u32,
        };
        let mut bytes = header.to_bytes().to_vec();
        bytes.extend_from_slice(static_meta.as_bytes());
        bytes.extend_from_slice(&((MAX_JSON_BLOCK_BYTES as u32) + 1).to_le_bytes());

        let mut buf = Cursor::new(bytes);
        let err = read_file(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            IoError::SizeLimitExceeded {
                context: "metadata JSON block",
                ..
            }
        ));
    }

    #[test]
    fn test_read_rejects_invalid_dynamic_metadata_values() {
        let static_meta_obj = StaticMeta::default_delivery(1000.0, 400.0);
        let static_meta = qdrv_meta::to_json(&static_meta_obj).unwrap();
        let invalid_dynamic = DynamicMeta::new(0, 100.0, 200.0);
        let dynamic_json = qdrv_meta::to_json(&invalid_dynamic).unwrap();

        let header = FileHeader {
            version: CURRENT_FORMAT_VERSION,
            tier: TIER_DELIVERY,
            codec: CODEC_RAW,
            width: 1,
            height: 1,
            frame_count: 1,
            static_meta_len: static_meta.len() as u32,
        };

        let mut bytes = header.to_bytes().to_vec();
        bytes.extend_from_slice(static_meta.as_bytes());
        bytes.extend_from_slice(&(dynamic_json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(dynamic_json.as_bytes());
        // One delivery raw pixel (RGB f32 LE) for completeness.
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
        bytes.extend_from_slice(&0.0f32.to_le_bytes());

        let mut buf = Cursor::new(bytes);
        let err = read_file(&mut buf).unwrap_err();
        assert!(matches!(err, IoError::InvalidMetadata(_)));
    }

    #[test]
    fn test_read_rejects_oversized_compressed_payload_length() {
        let static_meta = qdrv_meta::to_json(&StaticMeta::default_delivery(1000.0, 400.0)).unwrap();
        let dynamic_meta = qdrv_meta::to_json(&DynamicMeta::new(0, 1000.0, 400.0)).unwrap();
        let header = FileHeader {
            version: CURRENT_FORMAT_VERSION,
            tier: TIER_DELIVERY,
            codec: CODEC_AV1,
            width: 1,
            height: 1,
            frame_count: 1,
            static_meta_len: static_meta.len() as u32,
        };
        let mut bytes = header.to_bytes().to_vec();
        bytes.extend_from_slice(static_meta.as_bytes());
        bytes.extend_from_slice(&(dynamic_meta.len() as u32).to_le_bytes());
        bytes.extend_from_slice(dynamic_meta.as_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());

        let mut buf = Cursor::new(bytes);
        let err = read_file(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            IoError::SizeLimitExceeded {
                context: "compressed pixel payload",
                ..
            }
        ));
    }

    #[test]
    fn test_read_rejects_oversized_frame_area() {
        let static_meta = qdrv_meta::to_json(&StaticMeta::default_delivery(1000.0, 400.0)).unwrap();
        let header = FileHeader {
            version: CURRENT_FORMAT_VERSION,
            tier: TIER_DELIVERY,
            codec: CODEC_AV1,
            width: 16384,
            height: 16384,
            frame_count: 0,
            static_meta_len: static_meta.len() as u32,
        };
        let mut bytes = header.to_bytes().to_vec();
        bytes.extend_from_slice(static_meta.as_bytes());

        let mut buf = Cursor::new(bytes);
        let err = read_file(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            IoError::SizeLimitExceeded {
                context: "frame area pixels",
                ..
            }
        ));
    }

    #[test]
    fn test_read_accepts_container_v1() {
        let meta = StaticMeta::default_delivery(800.0, 300.0);
        let frame = delivery_frame(0, 2, 2, 0.2, 0.3, 0.4);
        let av1_cfg = Av1Config {
            speed: 10,
            quantizer: 0,
            lossless: true,
            threads: 1,
            chroma: ChromaSampling420::Cs444,
        };
        let mut buf = Cursor::new(Vec::<u8>::new());

        write_delivery_file_with_options(
            &mut buf,
            2,
            2,
            &meta,
            &[frame],
            &av1_cfg,
            ContainerWriteOptions {
                container_version: CONTAINER_VERSION_V1,
            },
        )
        .unwrap();

        buf.set_position(0);
        let qdrv = read_file(&mut buf).unwrap();
        assert_eq!(qdrv.header.version, CONTAINER_VERSION_V1);
        assert_eq!(qdrv.frames.len(), 1);
    }

    #[test]
    fn test_read_accepts_container_v2() {
        let meta = StaticMeta::default_delivery(800.0, 300.0);
        let frame = delivery_frame(0, 2, 2, 0.2, 0.3, 0.4);
        let av1_cfg = Av1Config {
            speed: 10,
            quantizer: 0,
            lossless: true,
            threads: 1,
            chroma: ChromaSampling420::Cs444,
        };
        let mut buf = Cursor::new(Vec::<u8>::new());
        write_delivery_file(&mut buf, 2, 2, &meta, &[frame], &av1_cfg).unwrap();

        buf.set_position(0);
        let qdrv = read_file(&mut buf).unwrap();
        assert_eq!(qdrv.header.version, CONTAINER_VERSION_V2);
        assert_eq!(qdrv.frames.len(), 1);
    }

    #[test]
    fn test_read_rejects_future_container_version() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"QDRV");
        bytes[4..6].copy_from_slice(&(CONTAINER_VERSION_V2 + 1).to_le_bytes());
        bytes[6] = TIER_DELIVERY;
        bytes[7] = CODEC_AV1;
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&0u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&0u32.to_le_bytes());

        let mut buf = Cursor::new(bytes.to_vec());
        let err = read_file(&mut buf).unwrap_err();
        assert!(matches!(err, IoError::FutureVersion(v) if v == CONTAINER_VERSION_V2 + 1));
    }
}
