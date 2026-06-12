// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Validation for checked-in corpus files under `test-vectors/`.
//!
//! These tests ensure the repository includes deterministic binary artefacts and
//! that the reader/writer stack can parse and round-trip them reliably.

use std::{fs, io::Cursor, path::PathBuf};

use qdrv_codec::{Av1Config, ChromaSampling420, MasteringCodec};
use qdrv_core::pq::{PQ_MAX_NITS, pq_oetf_f32};
use qdrv_io::{
    QdrvFile, read_file,
    writer::{DeliveryFrame, MasteringFrame, write_delivery_file, write_mastering_file},
};
use qdrv_meta::sha256_hex;

const DELIVERY_FILE: &str = "ramp-delivery.qdrv32";
const MASTERING_FILE: &str = "ramp-mastering.qdrv64";

const DELIVERY_SHA256: &str = "2a17a0333260c93476111f162ca8f1e72fc22d745f4cb3bd33e47c3fae548c79";
const MASTERING_SHA256: &str = "0ea98a2e05db07427c9189b30281d76c20ff87670d3c768785ffd7e99e697498";

const DELIVERY_TOLERANCE: f32 = 2.0 / 4095.0 + f32::EPSILON * 10.0;

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test-vectors")
}

fn read_vector_bytes(name: &str) -> Vec<u8> {
    let path = vectors_dir().join(name);
    fs::read(&path)
        .unwrap_or_else(|e| panic!("failed reading test vector '{}': {e}", path.display()))
}

fn parse_vector(name: &str) -> QdrvFile {
    let bytes = read_vector_bytes(name);
    let mut cursor = Cursor::new(bytes);
    read_file(&mut cursor)
        .unwrap_or_else(|e| panic!("failed parsing checked-in vector '{name}': {e}"))
}

fn assert_delivery_pixels_close(
    expected: &[qdrv_core::pixel::Pixel32],
    actual: &[qdrv_core::pixel::Pixel32],
) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "delivery pixel count mismatch: expected {}, got {}",
        expected.len(),
        actual.len()
    );
    for (idx, (lhs, rhs)) in expected.iter().zip(actual.iter()).enumerate() {
        assert!(
            (lhs.r - rhs.r).abs() <= DELIVERY_TOLERANCE,
            "delivery pixel {idx} R delta too high: {} vs {}",
            lhs.r,
            rhs.r
        );
        assert!(
            (lhs.g - rhs.g).abs() <= DELIVERY_TOLERANCE,
            "delivery pixel {idx} G delta too high: {} vs {}",
            lhs.g,
            rhs.g
        );
        assert!(
            (lhs.b - rhs.b).abs() <= DELIVERY_TOLERANCE,
            "delivery pixel {idx} B delta too high: {} vs {}",
            lhs.b,
            rhs.b
        );
    }
}

#[test]
fn checked_in_vectors_have_expected_sha256() {
    let delivery = read_vector_bytes(DELIVERY_FILE);
    let mastering = read_vector_bytes(MASTERING_FILE);

    assert_eq!(
        sha256_hex(&delivery),
        DELIVERY_SHA256,
        "delivery vector hash changed unexpectedly"
    );
    assert_eq!(
        sha256_hex(&mastering),
        MASTERING_SHA256,
        "mastering vector hash changed unexpectedly"
    );
}

#[test]
fn checked_in_delivery_vector_parses_expected_geometry_and_levels() {
    let qdrv = parse_vector(DELIVERY_FILE);
    assert!(qdrv.is_delivery());
    assert_eq!(qdrv.width(), 16);
    assert_eq!(qdrv.height(), 4);
    assert_eq!(qdrv.frame_count(), 1);
    assert_eq!(qdrv.frames.len(), 1);

    let pixels = qdrv.frames[0]
        .pixels
        .as_delivery()
        .expect("delivery vector should decode as delivery-tier pixels");
    assert_eq!(pixels.len(), 64);

    let expected_1000_pq = pq_oetf_f32((1000.0 / PQ_MAX_NITS) as f32);
    let end_of_first_row = &pixels[15];

    // Chroma round-trips can introduce a small bias near black; the first pixel
    // should still remain close to absolute black.
    assert!(
        pixels[0].r.abs() <= DELIVERY_TOLERANCE,
        "first pixel R too large: {}",
        pixels[0].r
    );
    assert!(
        pixels[0].g.abs() <= DELIVERY_TOLERANCE,
        "first pixel G too large: {}",
        pixels[0].g
    );
    assert!(
        pixels[0].b.abs() <= DELIVERY_TOLERANCE,
        "first pixel B too large: {}",
        pixels[0].b
    );

    assert!(
        (end_of_first_row.r - expected_1000_pq).abs() <= DELIVERY_TOLERANCE,
        "last column R not near 1000-nit PQ: expected ~{expected_1000_pq}, got {}",
        end_of_first_row.r
    );
    assert!(
        (end_of_first_row.g - expected_1000_pq).abs() <= DELIVERY_TOLERANCE,
        "last column G not near 1000-nit PQ: expected ~{expected_1000_pq}, got {}",
        end_of_first_row.g
    );
    assert!(
        (end_of_first_row.b - expected_1000_pq).abs() <= DELIVERY_TOLERANCE,
        "last column B not near 1000-nit PQ: expected ~{expected_1000_pq}, got {}",
        end_of_first_row.b
    );
}

#[test]
fn checked_in_mastering_vector_parses_expected_geometry_and_levels() {
    let qdrv = parse_vector(MASTERING_FILE);
    assert!(qdrv.is_mastering());
    assert_eq!(qdrv.width(), 16);
    assert_eq!(qdrv.height(), 4);
    assert_eq!(qdrv.frame_count(), 1);
    assert_eq!(qdrv.frames.len(), 1);

    let pixels = qdrv.frames[0]
        .pixels
        .as_mastering()
        .expect("mastering vector should decode as mastering-tier pixels");
    assert_eq!(pixels.len(), 64);

    assert_eq!(pixels[0].r, 0.0);
    assert_eq!(pixels[0].g, 0.0);
    assert_eq!(pixels[0].b, 0.0);

    assert_eq!(pixels[15].r, 1000.0);
    assert_eq!(pixels[15].g, 1000.0);
    assert_eq!(pixels[15].b, 1000.0);
}

#[test]
fn checked_in_vectors_roundtrip_through_writer_and_reader() {
    let delivery = parse_vector(DELIVERY_FILE);
    let delivery_frames: Vec<DeliveryFrame> = delivery
        .frames
        .iter()
        .map(|frame| DeliveryFrame {
            dynamic_meta: frame.dynamic_meta.clone(),
            pixels: frame
                .pixels
                .as_delivery()
                .expect("delivery frame must contain delivery-tier pixels")
                .to_vec(),
        })
        .collect();

    let av1_cfg = Av1Config {
        speed: 10,
        quantizer: 0,
        lossless: true,
        threads: 1,
        chroma: ChromaSampling420::Cs444,
    };
    let mut delivery_buf = Cursor::new(Vec::<u8>::new());
    write_delivery_file(
        &mut delivery_buf,
        delivery.width(),
        delivery.height(),
        &delivery.static_meta,
        &delivery_frames,
        &av1_cfg,
    )
    .expect("delivery round-trip encode should succeed");
    delivery_buf.set_position(0);
    let delivery_roundtrip =
        read_file(&mut delivery_buf).expect("delivery round-trip decode failed");

    assert!(delivery_roundtrip.is_delivery());
    assert_eq!(delivery_roundtrip.width(), delivery.width());
    assert_eq!(delivery_roundtrip.height(), delivery.height());
    assert_eq!(delivery_roundtrip.frame_count(), delivery.frame_count());
    assert_eq!(delivery_roundtrip.static_meta, delivery.static_meta);
    for (orig, regen) in delivery.frames.iter().zip(delivery_roundtrip.frames.iter()) {
        assert_eq!(orig.dynamic_meta, regen.dynamic_meta);
        assert_delivery_pixels_close(
            orig.pixels
                .as_delivery()
                .expect("original delivery frame should be delivery-tier"),
            regen
                .pixels
                .as_delivery()
                .expect("round-tripped delivery frame should be delivery-tier"),
        );
    }

    let mastering = parse_vector(MASTERING_FILE);
    let mastering_frames: Vec<MasteringFrame> = mastering
        .frames
        .iter()
        .map(|frame| MasteringFrame {
            dynamic_meta: frame.dynamic_meta.clone(),
            pixels: frame
                .pixels
                .as_mastering()
                .expect("mastering frame must contain mastering-tier pixels")
                .to_vec(),
        })
        .collect();
    let mut mastering_buf = Cursor::new(Vec::<u8>::new());
    write_mastering_file(
        &mut mastering_buf,
        mastering.width(),
        mastering.height(),
        &mastering.static_meta,
        &mastering_frames,
        MasteringCodec::Fpzip,
    )
    .expect("mastering round-trip encode should succeed");
    mastering_buf.set_position(0);
    let mastering_roundtrip =
        read_file(&mut mastering_buf).expect("mastering round-trip decode should succeed");

    assert!(mastering_roundtrip.is_mastering());
    assert_eq!(mastering_roundtrip.width(), mastering.width());
    assert_eq!(mastering_roundtrip.height(), mastering.height());
    assert_eq!(mastering_roundtrip.frame_count(), mastering.frame_count());
    assert_eq!(mastering_roundtrip.static_meta, mastering.static_meta);
    for (orig, regen) in mastering
        .frames
        .iter()
        .zip(mastering_roundtrip.frames.iter())
    {
        assert_eq!(orig.dynamic_meta, regen.dynamic_meta);
        assert_eq!(
            orig.pixels
                .as_mastering()
                .expect("original mastering frame should be mastering-tier"),
            regen
                .pixels
                .as_mastering()
                .expect("round-tripped mastering frame should be mastering-tier")
        );
    }
}
