// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! AVIF still-image ISOBMFF writer.
//!
//! The writer emits a single primary AV1 image item plus an optional QDRV JSON
//! metadata item inside the HEIF/AVIF `meta` box. Pixel data and metadata bytes
//! live in `mdat` and are referenced via `iloc`, keeping the structure compact
//! while preserving the same HDR `colr nclx` signalling used by the MP4 path.

use std::io::Write;

use crate::{
    MuxError, box_size_u32, build_av1c, build_colr_nclx, mdat_header_size, write_mdat_header,
};

const AVIF_PRIMARY_ITEM_ID: u16 = 1;
const QDRV_METADATA_ITEM_ID: u16 = 2;

/// Configuration for writing a single-frame AVIF still image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvifConfig {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

impl AvifConfig {
    /// Constructs an AVIF writer config from explicit image dimensions.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy)]
struct ItemLocation {
    item_id: u16,
    offset: u64,
    length: u64,
}

/// Writes a single-image AVIF file containing one AV1 still-picture item.
///
/// `av1_data` must be a complete AV1 still-picture item payload. When
/// `qdrv_metadata_json` is supplied, it is written as a MIME metadata item with
/// content type `application/qdrv+json` and linked to the primary image through
/// an item-reference box.
pub fn write_avif<W: Write>(
    writer: &mut W,
    config: &AvifConfig,
    av1_data: &[u8],
    qdrv_metadata_json: Option<&[u8]>,
) -> Result<(), MuxError> {
    if config.width == 0 || config.height == 0 {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "AVIF image dimensions must be non-zero, got {}x{}",
                config.width, config.height
            ),
        )));
    }
    if av1_data.is_empty() {
        return Err(MuxError::NoFrames);
    }
    if matches!(qdrv_metadata_json, Some(data) if data.is_empty()) {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "QDRV AVIF metadata item must not be empty",
        )));
    }

    let ftyp = build_avif_ftyp()?;
    let metadata_len = qdrv_metadata_json.map_or(0usize, <[u8]>::len);
    let payload_len = checked_add_u64(
        usize_to_u64(av1_data.len(), "AVIF AV1 item length")?,
        usize_to_u64(metadata_len, "AVIF metadata item length")?,
        "AVIF mdat payload length",
    )?;
    let mdat_header_len = u64::from(mdat_header_size(payload_len));

    let provisional_meta = build_meta(config, av1_data.len(), qdrv_metadata_json, 0, 0)?;
    let av1_offset = checked_add_u64(
        checked_add_u64(
            usize_to_u64(ftyp.len(), "AVIF ftyp length")?,
            usize_to_u64(provisional_meta.len(), "AVIF meta length")?,
            "AVIF pre-mdat offset",
        )?,
        mdat_header_len,
        "AVIF AV1 item offset",
    )?;
    let metadata_offset = checked_add_u64(
        av1_offset,
        usize_to_u64(av1_data.len(), "AVIF AV1 item length")?,
        "AVIF metadata item offset",
    )?;
    let meta = build_meta(
        config,
        av1_data.len(),
        qdrv_metadata_json,
        av1_offset,
        metadata_offset,
    )?;

    writer.write_all(&ftyp)?;
    writer.write_all(&meta)?;
    write_mdat_header(writer, payload_len)?;
    writer.write_all(av1_data)?;
    if let Some(metadata) = qdrv_metadata_json {
        writer.write_all(metadata)?;
    }
    Ok(())
}

fn build_avif_ftyp() -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::with_capacity(24);
    payload.extend_from_slice(b"avif");
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(b"avif");
    payload.extend_from_slice(b"mif1");
    payload.extend_from_slice(b"miaf");
    payload.extend_from_slice(b"avis");
    make_box(b"ftyp", &payload, "AVIF ftyp box")
}

fn build_meta(
    config: &AvifConfig,
    av1_len: usize,
    qdrv_metadata_json: Option<&[u8]>,
    av1_offset: u64,
    metadata_offset: u64,
) -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&build_hdlr_pict()?);
    payload.extend_from_slice(&build_pitm()?);

    let av1_location = ItemLocation {
        item_id: AVIF_PRIMARY_ITEM_ID,
        offset: av1_offset,
        length: usize_to_u64(av1_len, "AVIF AV1 item length")?,
    };
    if let Some(metadata) = qdrv_metadata_json {
        let metadata_location = ItemLocation {
            item_id: QDRV_METADATA_ITEM_ID,
            offset: metadata_offset,
            length: usize_to_u64(metadata.len(), "AVIF metadata item length")?,
        };
        payload.extend_from_slice(&build_iloc(&[av1_location, metadata_location])?);
        payload.extend_from_slice(&build_iinf(true)?);
        payload.extend_from_slice(&build_iref_metadata()?);
    } else {
        payload.extend_from_slice(&build_iloc(&[av1_location])?);
        payload.extend_from_slice(&build_iinf(false)?);
    }
    payload.extend_from_slice(&build_iprp(config)?);
    make_box(b"meta", &payload, "AVIF meta box")
}

fn build_hdlr_pict() -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(b"pict");
    payload.extend_from_slice(&[0u8; 12]);
    payload.extend_from_slice(b"QDRV AVIF\0");
    make_full_box(b"hdlr", 0, 0, &payload, "AVIF hdlr box")
}

fn build_pitm() -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::with_capacity(2);
    payload.extend_from_slice(&AVIF_PRIMARY_ITEM_ID.to_be_bytes());
    make_full_box(b"pitm", 0, 0, &payload, "AVIF pitm box")
}

fn build_iloc(locations: &[ItemLocation]) -> Result<Vec<u8>, MuxError> {
    let item_count = u16::try_from(locations.len()).map_err(|_| MuxError::SizeOverflow {
        context: "AVIF iloc item count",
    })?;
    let mut payload = Vec::new();
    payload.push(0x08); // offset_size = 0, length_size = 8
    payload.push(0x80); // base_offset_size = 8, index_size = 0
    payload.extend_from_slice(&item_count.to_be_bytes());
    for location in locations {
        payload.extend_from_slice(&location.item_id.to_be_bytes());
        payload.extend_from_slice(&0u16.to_be_bytes()); // construction_method = file offset
        payload.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index = self
        payload.extend_from_slice(&location.offset.to_be_bytes());
        payload.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        payload.extend_from_slice(&location.length.to_be_bytes());
    }
    make_full_box(b"iloc", 1, 0, &payload, "AVIF iloc box")
}

fn build_iinf(has_metadata: bool) -> Result<Vec<u8>, MuxError> {
    let entry_count = if has_metadata { 2u16 } else { 1u16 };
    let mut payload = Vec::new();
    payload.extend_from_slice(&entry_count.to_be_bytes());
    payload.extend_from_slice(&build_infe(
        AVIF_PRIMARY_ITEM_ID,
        b"av01",
        b"QDRV AV1 image\0",
        None,
    )?);
    if has_metadata {
        payload.extend_from_slice(&build_infe(
            QDRV_METADATA_ITEM_ID,
            b"mime",
            b"QDRV dynamic metadata\0",
            Some(b"application/qdrv+json\0"),
        )?);
    }
    make_full_box(b"iinf", 0, 0, &payload, "AVIF iinf box")
}

fn build_infe(
    item_id: u16,
    item_type: &[u8; 4],
    item_name: &[u8],
    content_type: Option<&[u8]>,
) -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&item_id.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
    payload.extend_from_slice(item_type);
    payload.extend_from_slice(item_name);
    if let Some(content_type) = content_type {
        payload.extend_from_slice(content_type);
        payload.push(0); // empty content_encoding
    }
    make_full_box(b"infe", 2, 0, &payload, "AVIF infe box")
}

fn build_iref_metadata() -> Result<Vec<u8>, MuxError> {
    let mut cdsc_payload = Vec::with_capacity(6);
    cdsc_payload.extend_from_slice(&QDRV_METADATA_ITEM_ID.to_be_bytes());
    cdsc_payload.extend_from_slice(&1u16.to_be_bytes());
    cdsc_payload.extend_from_slice(&AVIF_PRIMARY_ITEM_ID.to_be_bytes());
    let cdsc = make_box(b"cdsc", &cdsc_payload, "AVIF cdsc item reference")?;
    make_full_box(b"iref", 0, 0, &cdsc, "AVIF iref box")
}

fn build_iprp(config: &AvifConfig) -> Result<Vec<u8>, MuxError> {
    let mut ipco_payload = Vec::new();
    ipco_payload.extend_from_slice(&build_ispe(config)?);
    ipco_payload.extend_from_slice(&build_pixi()?);
    ipco_payload.extend_from_slice(&build_av1c());
    ipco_payload.extend_from_slice(&build_colr_nclx());
    let ipco = make_box(b"ipco", &ipco_payload, "AVIF ipco box")?;

    let ipma = build_ipma()?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&ipco);
    payload.extend_from_slice(&ipma);
    make_box(b"iprp", &payload, "AVIF iprp box")
}

fn build_ispe(config: &AvifConfig) -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&config.width.to_be_bytes());
    payload.extend_from_slice(&config.height.to_be_bytes());
    make_full_box(b"ispe", 0, 0, &payload, "AVIF ispe box")
}

fn build_pixi() -> Result<Vec<u8>, MuxError> {
    let payload = [3u8, 12, 12, 12];
    make_full_box(b"pixi", 0, 0, &payload, "AVIF pixi box")
}

fn build_ipma() -> Result<Vec<u8>, MuxError> {
    let mut payload = Vec::with_capacity(11);
    payload.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    payload.extend_from_slice(&AVIF_PRIMARY_ITEM_ID.to_be_bytes());
    payload.push(4); // association_count
    payload.push(0x81); // property 1: ispe, essential
    payload.push(0x82); // property 2: pixi, essential
    payload.push(0x83); // property 3: av1C, essential
    payload.push(0x04); // property 4: colr
    make_full_box(b"ipma", 0, 0, &payload, "AVIF ipma box")
}

fn make_full_box(
    typ: &[u8; 4],
    version: u8,
    flags: u32,
    payload: &[u8],
    context: &'static str,
) -> Result<Vec<u8>, MuxError> {
    if flags > 0x00FF_FFFF {
        return Err(MuxError::SizeOverflow { context });
    }
    let mut full_payload = Vec::with_capacity(payload.len().saturating_add(4));
    full_payload.push(version);
    full_payload.extend_from_slice(&(flags & 0x00FF_FFFF).to_be_bytes()[1..4]);
    full_payload.extend_from_slice(payload);
    make_box(typ, &full_payload, context)
}

fn make_box(typ: &[u8; 4], payload: &[u8], context: &'static str) -> Result<Vec<u8>, MuxError> {
    let size = box_size_u32(payload.len(), context)?;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(payload);
    Ok(out)
}

fn usize_to_u64(value: usize, context: &'static str) -> Result<u64, MuxError> {
    u64::try_from(value).map_err(|_| MuxError::SizeOverflow { context })
}

fn checked_add_u64(a: u64, b: u64, context: &'static str) -> Result<u64, MuxError> {
    a.checked_add(b).ok_or(MuxError::SizeOverflow { context })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find_child_box;

    fn find_top_level(data: &[u8], typ: &[u8; 4]) -> Option<(usize, usize)> {
        find_child_box(data, 0, data.len(), typ)
    }

    #[test]
    fn avif_writer_emits_brands_and_item_metadata() {
        let mut out = Vec::new();
        let av1 = [0x12, 0x00, 0x0A, 0x0A];
        let metadata = br#"{"qdrv":"metadata"}"#;
        write_avif(&mut out, &AvifConfig::new(64, 64), &av1, Some(metadata))
            .expect("avif write must succeed");

        assert_eq!(&out[4..8], b"ftyp");
        assert!(out.windows(4).any(|w| w == b"avif"));
        assert!(out.windows(4).any(|w| w == b"mif1"));
        assert!(out.windows(4).any(|w| w == b"avis"));

        let (meta_start, meta_end) = find_top_level(&out, b"meta").expect("meta box present");
        let meta = &out[meta_start..meta_end];
        assert!(find_child_box(meta, 12, meta.len(), b"pitm").is_some());
        assert!(find_child_box(meta, 12, meta.len(), b"iloc").is_some());
        assert!(find_child_box(meta, 12, meta.len(), b"iinf").is_some());
        assert!(find_child_box(meta, 12, meta.len(), b"iref").is_some());
        assert!(find_child_box(meta, 12, meta.len(), b"iprp").is_some());
        assert!(meta.windows(4).any(|w| w == b"av01"));
        assert!(
            meta.windows(b"application/qdrv+json".len())
                .any(|w| { w == b"application/qdrv+json" })
        );

        let (mdat_start, mdat_end) = find_top_level(&out, b"mdat").expect("mdat box present");
        let payload = &out[mdat_start + 8..mdat_end];
        assert!(payload.starts_with(&av1));
        assert_eq!(&payload[av1.len()..], metadata);
    }

    #[test]
    fn avif_writer_rejects_empty_image_payload() {
        let mut out = Vec::new();
        assert!(write_avif(&mut out, &AvifConfig::new(64, 64), &[], None).is_err());
    }

    #[test]
    fn avif_writer_rejects_zero_dimensions() {
        let mut out = Vec::new();
        assert!(write_avif(&mut out, &AvifConfig::new(0, 64), &[1], None).is_err());
        assert!(write_avif(&mut out, &AvifConfig::new(64, 0), &[1], None).is_err());
    }
}
