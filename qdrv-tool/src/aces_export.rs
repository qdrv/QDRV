// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! ACES OpenEXR sequence export for `qdrv aces-export`.

use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
};

use clap::ValueEnum;
use exr::prelude::{Image, SpecificChannels, Vec2, WritableImage};
use qdrv_core::{
    PQ_MAX_NITS, Pixel32, Pixel64, apply_odt_rec709_100nit, apply_odt_rec2020_1000nit,
    apply_odt_rec2020_4000nit, apply_rrt, pq::pq_eotf_f32, rec2020_to_aces_ap0,
};
use qdrv_io::{
    container::{TIER_DELIVERY, TIER_MASTERING},
    reader::{PixelBuffer, QdrvStreamReader},
};

use crate::{TempFileGuard, part_path};

/// Output transform for `qdrv aces-export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AcesExportTargetArg {
    /// Scene-linear ACES2065-1 / AP0 EXR, with no RRT/ODT applied.
    #[value(name = "aces2065-1")]
    Aces2065_1,
    /// ACES RRT plus Rec.709 100 nit dim-surround ODT.
    #[value(name = "rec709-100nit")]
    Rec709100Nit,
    /// ACES v1.3 Rec.2020 ST2084 1000 nit RRT+ODT output transform.
    #[value(name = "rec2020-1000nit")]
    Rec20201000Nit,
    /// ACES v1.3 Rec.2020 ST2084 4000 nit RRT+ODT output transform.
    #[value(name = "rec2020-4000nit")]
    Rec20204000Nit,
}

impl AcesExportTargetArg {
    fn label(self) -> &'static str {
        match self {
            Self::Aces2065_1 => "ACES2065-1 scene-linear AP0",
            Self::Rec709100Nit => "ACES RRT + Rec.709 100 nit dim-surround ODT",
            Self::Rec20201000Nit => "ACES Rec.2020 ST2084 1000 nit RRT+ODT",
            Self::Rec20204000Nit => "ACES Rec.2020 ST2084 4000 nit RRT+ODT",
        }
    }
}

pub(crate) struct AcesExportOptions<'a> {
    pub input: &'a Path,
    pub output_dir: &'a Path,
    pub target: AcesExportTargetArg,
    pub reference_white_nits: f64,
    pub prefix: &'a str,
    pub start_number: u32,
}

pub(crate) fn cmd_aces_export(
    opts: AcesExportOptions<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_reference_white(opts.reference_white_nits)?;
    validate_prefix(opts.prefix)?;

    fs::create_dir_all(opts.output_dir).map_err(|e| {
        format!(
            "cannot create output directory '{}': {e}",
            opts.output_dir.display()
        )
    })?;
    let output_meta = fs::metadata(opts.output_dir).map_err(|e| {
        format!(
            "cannot inspect output directory '{}': {e}",
            opts.output_dir.display()
        )
    })?;
    if !output_meta.is_dir() {
        return Err(format!(
            "output path '{}' exists but is not a directory",
            opts.output_dir.display()
        )
        .into());
    }

    let file = File::open(opts.input)
        .map_err(|e| format!("cannot open '{}': {e}", opts.input.display()))?;
    let mut stream =
        QdrvStreamReader::new(BufReader::new(file)).map_err(|e| format!("read error: {e}"))?;
    let header = stream.header().clone();
    if header.tier != TIER_DELIVERY && header.tier != TIER_MASTERING {
        return Err(format!(
            "aces-export requires a delivery or mastering QDRV stream; '{}' has tier byte {}",
            opts.input.display(),
            header.tier
        )
        .into());
    }
    if header.frame_count == 0 {
        return Err("aces-export requires at least one frame".into());
    }

    let width = usize::try_from(header.width).map_err(|_| "frame width does not fit into usize")?;
    let height =
        usize::try_from(header.height).map_err(|_| "frame height does not fit into usize")?;
    let expected_pixels = width
        .checked_mul(height)
        .ok_or("frame dimensions overflow usize")?;

    let mut written = 0usize;
    while let Some(frame) = stream
        .next_frame()
        .map_err(|e| format!("read error: {e}"))?
    {
        let pixels = transform_frame_pixels(
            &frame.pixels,
            expected_pixels,
            opts.reference_white_nits,
            opts.target,
            written,
        )?;
        let frame_number = opts
            .start_number
            .checked_add(u32::try_from(written).map_err(|_| "frame index exceeds u32")?)
            .ok_or("frame numbering overflows u32")?;
        let output_path = opts
            .output_dir
            .join(format!("{}_{frame_number:06}.exr", opts.prefix));
        atomic_write_exr(&output_path, width, height, &pixels)?;
        written += 1;
    }

    println!(
        "ACES export: {} -> {}",
        opts.input.display(),
        opts.output_dir.display()
    );
    println!("  Frames written       : {written}");
    println!(
        "  Dimensions           : {}x{}",
        header.width, header.height
    );
    println!("  Target transform     : {}", opts.target.label());
    println!(
        "  Reference white      : {:.4} nits",
        opts.reference_white_nits
    );
    println!("  Filename prefix      : {}", opts.prefix);
    Ok(())
}

fn validate_reference_white(reference_white_nits: f64) -> Result<(), Box<dyn std::error::Error>> {
    if !reference_white_nits.is_finite() || reference_white_nits <= 0.0 {
        return Err(format!(
            "--reference-white-nits must be a positive finite value (got {reference_white_nits})"
        )
        .into());
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), Box<dyn std::error::Error>> {
    if prefix.is_empty() {
        return Err("--prefix must not be empty".into());
    }
    if prefix.contains(['/', '\\', ':']) || prefix == "." || prefix == ".." {
        return Err("--prefix must be a file-name prefix, not a path".into());
    }
    Ok(())
}

fn transform_frame_pixels(
    pixels: &PixelBuffer,
    expected_pixels: usize,
    reference_white_nits: f64,
    target: AcesExportTargetArg,
    frame_index: usize,
) -> Result<Vec<[f32; 3]>, Box<dyn std::error::Error>> {
    if pixels.len() != expected_pixels {
        return Err(format!(
            "frame {frame_index}: pixel count mismatch, expected {expected_pixels}, got {}",
            pixels.len()
        )
        .into());
    }
    let mut out = Vec::new();
    out.try_reserve_exact(expected_pixels)
        .map_err(|_| format!("frame {frame_index}: cannot allocate ACES export pixel buffer"))?;

    match pixels {
        PixelBuffer::Delivery(delivery) => {
            for pixel in delivery {
                out.push(transform_aces_pixel(
                    delivery_to_aces_ap0(*pixel, reference_white_nits),
                    target,
                )?);
            }
        }
        PixelBuffer::Mastering(mastering) => {
            for pixel in mastering {
                out.push(transform_aces_pixel(
                    mastering_to_aces_ap0(*pixel, reference_white_nits),
                    target,
                )?);
            }
        }
    }
    Ok(out)
}

fn delivery_to_aces_ap0(pixel: Pixel32, reference_white_nits: f64) -> (f64, f64, f64) {
    let rec2020 = (
        f64::from(pq_eotf_f32(pixel.r)) * PQ_MAX_NITS / reference_white_nits,
        f64::from(pq_eotf_f32(pixel.g)) * PQ_MAX_NITS / reference_white_nits,
        f64::from(pq_eotf_f32(pixel.b)) * PQ_MAX_NITS / reference_white_nits,
    );
    rec2020_to_aces_ap0(rec2020)
}

fn mastering_to_aces_ap0(pixel: Pixel64, reference_white_nits: f64) -> (f64, f64, f64) {
    rec2020_to_aces_ap0((
        pixel.r / reference_white_nits,
        pixel.g / reference_white_nits,
        pixel.b / reference_white_nits,
    ))
}

fn transform_aces_pixel(
    aces_ap0: (f64, f64, f64),
    target: AcesExportTargetArg,
) -> Result<[f32; 3], Box<dyn std::error::Error>> {
    let transformed = match target {
        AcesExportTargetArg::Aces2065_1 => aces_ap0,
        AcesExportTargetArg::Rec709100Nit => apply_odt_rec709_100nit(apply_rrt(aces_ap0)),
        AcesExportTargetArg::Rec20201000Nit => apply_odt_rec2020_1000nit(aces_ap0),
        AcesExportTargetArg::Rec20204000Nit => apply_odt_rec2020_4000nit(aces_ap0),
    };
    if !transformed.0.is_finite() || !transformed.1.is_finite() || !transformed.2.is_finite() {
        return Err("ACES transform produced a non-finite channel".into());
    }
    Ok([
        transformed.0 as f32,
        transformed.1 as f32,
        transformed.2 as f32,
    ])
}

fn atomic_write_exr(
    path: &Path,
    width: usize,
    height: usize,
    pixels: &[[f32; 3]],
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = width
        .checked_mul(height)
        .ok_or("EXR frame dimensions overflow usize")?;
    if pixels.len() != expected {
        return Err(format!(
            "EXR pixel count mismatch: expected {expected}, got {}",
            pixels.len()
        )
        .into());
    }

    let tmp_guard = TempFileGuard::new(part_path(path));
    let mut file = File::create(tmp_guard.path())
        .map_err(|e| format!("cannot create '{}': {e}", tmp_guard.path().display()))?;
    let channels = SpecificChannels::rgb::<f32, f32, f32>(|Vec2(x, y)| {
        let pixel: [f32; 3] = pixels[y * width + x];
        (pixel[0], pixel[1], pixel[2])
    });
    let image = Image::from_channels((width, height), channels);
    image
        .write()
        .non_parallel()
        .to_unbuffered(&mut file)
        .map_err(|e| format!("EXR write error for '{}': {e}", tmp_guard.path().display()))?;
    file.sync_all()
        .map_err(|e| format!("fsync error for '{}': {e}", tmp_guard.path().display()))?;

    let tmp_path: PathBuf = tmp_guard.commit();
    fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use exr::prelude::{ReadChannels, ReadLayers};
    use qdrv_core::{
        REFERENCE_WHITE_NITS, aces_ap0_to_rec2020, metrics_for_delivery_frame, pq::nits_to_pq,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn delivery_reference_white_maps_to_near_unit_aces_white() {
        let pq = nits_to_pq(REFERENCE_WHITE_NITS).expect("reference white is inside PQ range");
        let aces = delivery_to_aces_ap0(
            Pixel32::new_unchecked(pq as f32, pq as f32, pq as f32),
            REFERENCE_WHITE_NITS,
        );
        assert!((aces.0 - 1.0).abs() < 1e-3, "R: {}", aces.0);
        assert!((aces.1 - 1.0).abs() < 1e-3, "G: {}", aces.1);
        assert!((aces.2 - 1.0).abs() < 1e-3, "B: {}", aces.2);
    }

    #[test]
    fn reject_path_like_prefixes() {
        assert!(validate_prefix("frame").is_ok());
        assert!(validate_prefix("../frame").is_err());
        assert!(validate_prefix("dir\\frame").is_err());
        assert!(validate_prefix("drive:frame").is_err());
    }

    #[test]
    fn atomic_exr_writer_emits_openexr_file() {
        let root = std::env::temp_dir().join(format!(
            "qdrv-aces-export-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("frame_000000.exr");
        atomic_write_exr(&path, 2, 1, &[[0.18, 0.18, 0.18], [1.0, 0.5, 0.25]])
            .expect("EXR write should succeed");

        let bytes = fs::read(&path).expect("read EXR output");
        assert_eq!(
            &bytes[0..4],
            &[0x76, 0x2f, 0x31, 0x01],
            "OpenEXR magic number must be present"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn aces2065_exr_interchange_roundtrip_preserves_delivery_fidelity() {
        let root = std::env::temp_dir().join(format!(
            "qdrv-aces-roundtrip-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("frame_000000.exr");

        let source = vec![
            pq_pixel(10.0, 10.0, 10.0),
            pq_pixel(
                REFERENCE_WHITE_NITS,
                REFERENCE_WHITE_NITS,
                REFERENCE_WHITE_NITS,
            ),
            pq_pixel(400.0, 80.0, 40.0),
            pq_pixel(50.0, 200.0, 600.0),
        ];
        let aces = transform_frame_pixels(
            &PixelBuffer::Delivery(source.clone()),
            source.len(),
            REFERENCE_WHITE_NITS,
            AcesExportTargetArg::Aces2065_1,
            0,
        )
        .expect("ACES2065-1 transform should succeed");
        atomic_write_exr(&path, 2, 2, &aces).expect("EXR write should succeed");

        let image = exr::prelude::read()
            .no_deep_data()
            .largest_resolution_level()
            .rgb_channels(
                |resolution, _| vec![[0.0_f32; 3]; resolution.width() * resolution.height()],
                |pixels, position, (r, g, b): (f32, f32, f32)| {
                    pixels[position.y() * 2 + position.x()] = [r, g, b];
                },
            )
            .first_valid_layer()
            .all_attributes()
            .from_file(&path)
            .expect("read exported EXR");
        let recovered: Vec<Pixel32> = image
            .layer_data
            .channel_data
            .pixels
            .into_iter()
            .map(aces2065_pixel_to_delivery)
            .collect();

        let metrics =
            metrics_for_delivery_frame(&source, &recovered).expect("fidelity metrics should exist");
        assert!(
            metrics.psnr_db.is_infinite() || metrics.psnr_db > 100.0,
            "PSNR after ACES2065-1 EXR roundtrip was {} dB",
            metrics.psnr_db
        );
        assert!(
            metrics.delta_e76 < 0.01,
            "DeltaE76 after ACES2065-1 EXR roundtrip was {}",
            metrics.delta_e76
        );
        let _ = fs::remove_dir_all(root);
    }

    fn pq_pixel(r_nits: f64, g_nits: f64, b_nits: f64) -> Pixel32 {
        Pixel32::new_unchecked(
            nits_to_pq(r_nits).expect("test nits inside PQ range") as f32,
            nits_to_pq(g_nits).expect("test nits inside PQ range") as f32,
            nits_to_pq(b_nits).expect("test nits inside PQ range") as f32,
        )
    }

    fn aces2065_pixel_to_delivery(pixel: [f32; 3]) -> Pixel32 {
        let rec2020 = aces_ap0_to_rec2020((
            f64::from(pixel[0]),
            f64::from(pixel[1]),
            f64::from(pixel[2]),
        ));
        Pixel32::new_unchecked(
            nits_to_pq((rec2020.0 * REFERENCE_WHITE_NITS).clamp(0.0, PQ_MAX_NITS))
                .expect("roundtripped red is inside PQ range") as f32,
            nits_to_pq((rec2020.1 * REFERENCE_WHITE_NITS).clamp(0.0, PQ_MAX_NITS))
                .expect("roundtripped green is inside PQ range") as f32,
            nits_to_pq((rec2020.2 * REFERENCE_WHITE_NITS).clamp(0.0, PQ_MAX_NITS))
                .expect("roundtripped blue is inside PQ range") as f32,
        )
    }
}
