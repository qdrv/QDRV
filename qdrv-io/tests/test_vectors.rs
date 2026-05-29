// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests that generate and validate formal QDRV test vector scenarios.
//!
//! Each test writes a deterministic QDRV file to an in-memory buffer, reads it
//! back, and asserts that the recovered pixel values match the expected results.
//! The scenarios correspond to the reference files described in `test-vectors/README_TEST_VECTORS.md`.

use std::io::Cursor;

use qdrv_codec::{Av1Config, MasteringCodec};
use qdrv_core::pixel::{Pixel32, Pixel64};
use qdrv_core::pq::{PQ_MAX_NITS, pq_oetf_f32};
use qdrv_io::container::{CONTAINER_VERSION_V1, CONTAINER_VERSION_V2, FileHeader, HEADER_SIZE};
use qdrv_io::reader::read_file;
use qdrv_io::writer::{
    ContainerWriteOptions, DeliveryFrame, MasteringFrame, write_delivery_file,
    write_delivery_file_with_options, write_mastering_file,
};
use qdrv_meta::{DynamicMeta, StaticMeta};

/// Builds a single-frame horizontal luminance ramp in **linear nits** (`Pixel64`), where
/// the left column is `0.0` nits and the right column approaches `1_000.0` nits. Each
/// row repeats the same ramp, matching the integration tests' expected mastering-tier
/// geometry (row-major order, grey-scale RGB triplets).
fn ramp_mastering_pixels(width: u32, height: u32) -> Vec<Pixel64> {
    debug_assert!(
        width > 0 && height > 0,
        "ramp_mastering_pixels requires non-zero dimensions, got {width}x{height}"
    );
    let pixel_count = (width as usize).saturating_mul(height as usize);
    (0..pixel_count)
        .map(|i| {
            let col = (i % width as usize) as f64;
            let nits = col / (width as f64 - 1.0).max(1.0) * 1000.0;
            Pixel64::new_unchecked(nits, nits, nits)
        })
        .collect()
}

/// Round-trips a mastering-tier ramp through `write_mastering_file` / `read_file`,
/// asserting the first and last columns survive fpzip lossless compression unchanged.
#[test]
fn test_vector_ramp_mastering() {
    let w = 16u32;
    let h = 4u32;
    let pixels = ramp_mastering_pixels(w, h);
    let meta = StaticMeta::default_mastering();
    let frames = vec![MasteringFrame {
        dynamic_meta: DynamicMeta::new(0, 1000.0, 500.0),
        pixels: pixels.clone(),
    }];

    let mut buf = Cursor::new(Vec::<u8>::new());
    write_mastering_file(&mut buf, w, h, &meta, &frames, MasteringCodec::Fpzip).unwrap();

    buf.set_position(0);
    let qdrv = read_file(&mut buf).unwrap();

    assert!(qdrv.is_mastering());
    let read_pixels = qdrv.frames[0].pixels.as_mastering().unwrap();

    assert_eq!(read_pixels[0].r, 0.0, "first pixel should be 0.0 nits");
    assert_eq!(read_pixels[0].g, 0.0);
    assert_eq!(read_pixels[0].b, 0.0);

    let last = &read_pixels[15];
    assert_eq!(last.r, 1000.0, "last column should be 1000.0 nits");
    assert_eq!(last.g, 1000.0);
}

/// Encodes the same ramp through the delivery pipeline (PQ + lossless AV1), then reads
/// the `.qdrv32` back and checks the first pixel's PQ channels against a tight tolerance.
#[test]
fn test_vector_ramp_delivery() {
    let w = 16u32;
    let h = 4u32;
    let mastering_pixels = ramp_mastering_pixels(w, h);
    let delivery_pixels: Vec<Pixel32> = mastering_pixels
        .iter()
        .map(|p| {
            Pixel32::new_unchecked(
                pq_oetf_f32((p.r / PQ_MAX_NITS) as f32),
                pq_oetf_f32((p.g / PQ_MAX_NITS) as f32),
                pq_oetf_f32((p.b / PQ_MAX_NITS) as f32),
            )
        })
        .collect();

    let meta = StaticMeta::default_delivery(1000.0, 500.0);
    let av1_cfg = Av1Config {
        speed: 10,
        quantizer: 0,
        lossless: true,
        threads: 1,
        ..Default::default()
    };
    let frames = vec![DeliveryFrame {
        dynamic_meta: DynamicMeta::new(0, 1000.0, 500.0),
        pixels: delivery_pixels,
    }];

    let mut buf = Cursor::new(Vec::<u8>::new());
    write_delivery_file(&mut buf, w, h, &meta, &frames, &av1_cfg).unwrap();

    buf.set_position(0);
    let qdrv = read_file(&mut buf).unwrap();

    assert!(qdrv.is_delivery());
    let read_pixels = qdrv.frames[0].pixels.as_delivery().unwrap();
    let tolerance = 2.0 / 4095.0 + f32::EPSILON * 10.0;

    // The first pixel corresponds to 0 nits. RGB->YCbCr->RGB with 12-bit chroma
    // quantisation can introduce a small near-black bias, so this tolerance
    // allows up to two 12-bit LSBs.
    assert!(
        read_pixels[0].r.abs() <= tolerance,
        "first pixel R too large: {}",
        read_pixels[0].r
    );
    assert!(
        read_pixels[0].g.abs() <= tolerance,
        "first pixel G too large: {}",
        read_pixels[0].g
    );
    assert!(
        read_pixels[0].b.abs() <= tolerance,
        "first pixel B too large: {}",
        read_pixels[0].b
    );
}

/// Verifies that extreme mastering-tier luminances above typical consumer ceilings are
/// preserved bit-for-bit when metadata ceilings are raised accordingly (no silent clipping).
#[test]
fn test_vector_above_ceiling_mastering() {
    let pixels = vec![Pixel64::new_unchecked(50_000.0, 25_000.0, 12_500.0)];
    let meta = StaticMeta::default_mastering();
    let frames = vec![MasteringFrame {
        dynamic_meta: DynamicMeta::new(0, 50_000.0, 50_000.0),
        pixels,
    }];

    let mut buf = Cursor::new(Vec::<u8>::new());
    write_mastering_file(&mut buf, 1, 1, &meta, &frames, MasteringCodec::Fpzip).unwrap();

    buf.set_position(0);
    let qdrv = read_file(&mut buf).unwrap();

    let p = &qdrv.frames[0].pixels.as_mastering().unwrap()[0];
    assert_eq!(p.r, 50_000.0);
    assert_eq!(p.g, 25_000.0);
    assert_eq!(p.b, 12_500.0);
}

#[test]
fn test_vector_neutral_grey_delivery() {
    let pq_203 = pq_oetf_f32((203.0 / PQ_MAX_NITS) as f32);
    let pixels = vec![Pixel32::new_unchecked(pq_203, pq_203, pq_203)];
    let meta = StaticMeta::default_delivery(203.0, 203.0);
    let av1_cfg = Av1Config {
        speed: 10,
        quantizer: 0,
        lossless: true,
        threads: 1,
        ..Default::default()
    };
    let frames = vec![DeliveryFrame {
        dynamic_meta: DynamicMeta::new(0, 203.0, 203.0),
        pixels,
    }];

    let mut buf = Cursor::new(Vec::<u8>::new());
    write_delivery_file(&mut buf, 1, 1, &meta, &frames, &av1_cfg).unwrap();

    buf.set_position(0);
    let qdrv = read_file(&mut buf).unwrap();

    let p = &qdrv.frames[0].pixels.as_delivery().unwrap()[0];
    let tolerance = 1.0 / 4095.0 + f32::EPSILON * 10.0;
    assert!(
        (p.r - pq_203).abs() <= tolerance,
        "PQ 203 nits: expected ~{pq_203}, got {}",
        p.r
    );
}

#[test]
fn test_writer_default_is_container_v2() {
    let meta = StaticMeta::default_delivery(1000.0, 500.0);
    let frame = DeliveryFrame {
        dynamic_meta: DynamicMeta::new(0, 1000.0, 500.0),
        pixels: vec![Pixel32::new_unchecked(0.2, 0.2, 0.2)],
    };
    let av1_cfg = Av1Config {
        speed: 10,
        quantizer: 0,
        lossless: true,
        threads: 1,
        ..Default::default()
    };

    let mut buf = Cursor::new(Vec::<u8>::new());
    write_delivery_file(&mut buf, 1, 1, &meta, &[frame], &av1_cfg).unwrap();
    let bytes = buf.into_inner();
    let mut header = [0u8; HEADER_SIZE];
    header.copy_from_slice(&bytes[..HEADER_SIZE]);
    let parsed = FileHeader::from_bytes(&header).unwrap();
    assert_eq!(parsed.version, CONTAINER_VERSION_V2);
}

#[test]
fn test_writer_can_emit_container_v1_compatibility() {
    let meta = StaticMeta::default_delivery(1000.0, 500.0);
    let frame = DeliveryFrame {
        dynamic_meta: DynamicMeta::new(0, 1000.0, 500.0),
        pixels: vec![Pixel32::new_unchecked(0.3, 0.3, 0.3)],
    };
    let av1_cfg = Av1Config {
        speed: 10,
        quantizer: 0,
        lossless: true,
        threads: 1,
        ..Default::default()
    };

    let mut buf = Cursor::new(Vec::<u8>::new());
    write_delivery_file_with_options(
        &mut buf,
        1,
        1,
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
