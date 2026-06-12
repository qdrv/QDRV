// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-io
//!
//! QDRV binary container format: reader and writer for `.qdrv32` (delivery)
//! and `.qdrv64` (mastering) files.
//!
//! Container version support:
//! - Reader: accepts v1 and v2.
//! - Writer: defaults to v2 and can emit v1 with explicit options.
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`container`] | File header definition, magic bytes, container version constants, codec / tier bytes |
//! | [`reader`] | [`read_file`], [`QdrvStreamReader`], bounded JSON / payload allocation, codec dispatch |
//! | [`writer`] | [`write_delivery_file`] / [`write_mastering_file`] and `*_with_options` variants, atomic-temp helpers |
//! | [`error`] | [`IoError`] enum |
//!
//! ## Codec modes
//!
//! | Codec byte | Pixel storage |
//! |------------|--------------|
//! | 0 | Raw uncompressed IEEE 754 bytes (testing only). |
//! | 1 | AV1 (delivery) / fpzip or ZFP reversible (mastering). |
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 or later (GPLv2+).

pub mod container;
pub mod error;
pub mod reader;
pub mod writer;

pub use error::IoError;
pub use reader::{PixelBuffer, QdrvFile, QdrvFrame, QdrvStreamReader, read_file};
pub use writer::{
    ContainerWriteOptions, DeliveryFrame, MasteringFrame, write_delivery_file,
    write_delivery_file_with_options, write_mastering_file, write_mastering_file_with_options,
};
