// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! AV1 encode and decode for the QDRV delivery tier.
//!
//! ## Encoding pipeline
//!
//! QDRV delivery-tier pixels are Float32 PQ-encoded RGB values in `[0.0, 1.0]`.
//! AV1 operates on integer YCbCr samples. The encode pipeline is:
//!
//! 1. Convert each `Pixel32` (RGB Float32 PQ) to `YCbCr32` using the
//!    ITU-R Rec. 2100 non-constant luminance (NCL) coefficients.
//! 2. Quantise the Float32 YCbCr values to 12-bit unsigned integers:
//!    - Y:  `[0.0, 1.0]`     → `[0, 4095]`
//!    - Cb: `[-0.5, 0.5]`    → `[0, 4095]` (offset by 2048)
//!    - Cr: `[-0.5, 0.5]`    → `[0, 4095]` (offset by 2048)
//! 3. Fill a rav1e `Frame<u16>` with the three planes.
//! 4. Encode as an AV1 still picture (independent, self-contained bitstream)
//!    using rav1e configured for:
//!    - 12-bit depth, 4:4:4 chroma sampling, full pixel range
//!    - ITU-R Rec. 2100 colour description (BT.2020 primaries, ST 2084 PQ
//!      transfer, BT.2020 NCL matrix)
//! 5. Concatenate all output packets (sequence header OBU + frame OBU) into
//!    a single `Vec<u8>` representing the complete per-frame AV1 bitstream.
//!
//! ## Decoding pipeline
//!
//! 1. Feed the AV1 bitstream to dav1d.
//! 2. Extract the Y, Cb, Cr planes from the decoded picture.
//! 3. Dequantise from 12-bit integers back to Float32 YCbCr.
//! 4. Convert each `YCbCr32` back to `Pixel32` (RGB Float32 PQ) using the
//!    Rec. 2100 NCL inverse transform.
//!
//! ## Lossless mode
//!
//! When `Av1Config::lossless` is `true`, the AV1 quantiser is set to 0 and
//! rav1e uses its lossless encoding path. This produces a lossless encoding
//! of the 12-bit YCbCr representation. Note that the Float32 → 12-bit
//! quantisation step (step 2 above) is always lossy; "lossless AV1" means
//! lossless *of the 12-bit values*, not of the original Float32 data.

use dav1d::{Decoder, PixelLayout, PlanarImageComponent, Settings};
use qdrv_core::pixel::{Pixel32, YCbCr32};
use rav1e::prelude::*;

use crate::error::CodecError;

// ---------------------------------------------------------------------------
// Encoder configuration
// ---------------------------------------------------------------------------

/// Chroma sampling mode for AV1 encoding.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ChromaSampling420 {
    /// 4:4:4 — full chroma resolution (default).
    #[default]
    Cs444,
    /// 4:2:0 — half chroma resolution in both axes (bandwidth-constrained delivery).
    Cs420,
}

/// Configuration for the QDRV AV1 encoder (rav1e).
#[derive(Debug, Clone)]
pub struct Av1Config {
    /// rav1e speed preset. Range `0` (slowest, best quality) to `10`
    /// (fastest, lowest quality). Default is `6`, which gives a good
    /// balance for dynamic-range mastering use cases.
    pub speed: u8,

    /// AV1 quantiser. Range `0` (lossless) to `255` (worst quality).
    /// For archival delivery values in `20–60` are typical.
    /// Ignored when `lossless` is `true`.
    pub quantizer: usize,

    /// If `true`, forces lossless encoding of the 12-bit YCbCr
    /// representation. The quantiser is set to `0` and rav1e uses its
    /// lossless mode. This is slower than lossy encoding.
    pub lossless: bool,

    /// Number of encoder threads. `0` = automatic (one thread per logical CPU).
    pub threads: usize,

    /// Chroma sampling mode. Default is 4:4:4.
    pub chroma: ChromaSampling420,
}

impl Default for Av1Config {
    fn default() -> Self {
        Self {
            speed: 6,
            quantizer: 40,
            lossless: false,
            threads: 0,
            chroma: ChromaSampling420::Cs444,
        }
    }
}

impl Av1Config {
    /// Validates documented AV1 configuration constraints.
    pub fn validate(&self) -> Result<(), CodecError> {
        if self.speed > 10 {
            return Err(CodecError::Av1Encode(format!(
                "Av1Config::speed must be in 0..=10, got {}",
                self.speed
            )));
        }
        if self.quantizer > 255 {
            return Err(CodecError::Av1Encode(format!(
                "Av1Config::quantizer must be in 0..=255, got {}",
                self.quantizer
            )));
        }
        if self.lossless && self.quantizer != 0 {
            return Err(CodecError::Av1Encode(format!(
                "Av1Config::lossless=true requires quantizer=0, got {}",
                self.quantizer
            )));
        }
        Ok(())
    }
}

/// Three planar 12-bit luma + chroma buffers produced by
/// [`subsample_420`]. Named so the public return type isn't a bare
/// tuple of three `Vec<u16>` (clippy `type_complexity`).
pub struct Planes420 {
    /// Luma plane (`width × height` samples; unchanged from input).
    pub y: Vec<u16>,
    /// Subsampled Cb plane (`(width/2) × (height/2)` samples).
    pub cb: Vec<u16>,
    /// Subsampled Cr plane (`(width/2) × (height/2)` samples).
    pub cr: Vec<u16>,
}

/// Downsamples Cb and Cr planes from 4:4:4 to 4:2:0 using a 2×2 box filter.
///
/// Y plane is returned unchanged. Cb and Cr are averaged over each 2×2 block.
/// All three input planes must hold exactly `width × height` samples, and
/// `width` and `height` must be **even** so every luma position participates
/// in a full 2×2 block (odd sizes would drop edge columns/rows of chroma).
pub fn subsample_420(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    width: usize,
    height: usize,
) -> Result<Planes420, CodecError> {
    let n = width.checked_mul(height).ok_or_else(|| {
        CodecError::Av1Encode("subsample_420: width × height overflows usize".into())
    })?;
    if y_plane.len() != n || cb_plane.len() != n || cr_plane.len() != n {
        return Err(CodecError::Av1Encode(format!(
            "subsample_420: expected {n} samples per full-resolution plane, got y={}, cb={}, cr={}",
            y_plane.len(),
            cb_plane.len(),
            cr_plane.len()
        )));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(CodecError::Av1Encode(format!(
            "subsample_420: even width and height required, got {width}×{height}"
        )));
    }

    let cw = width / 2;
    let ch = height / 2;
    let out_n = cw.checked_mul(ch).ok_or_else(|| {
        CodecError::Av1Encode("subsample_420: chroma plane size overflows usize".into())
    })?;
    // T-1: prefer try_reserve_exact to keep this in line with the rest
    // of the codec's allocation pattern. `out_n` is already bounded
    // against usize overflow above; this guards against OOM at the very
    // large but in-bound end of the input range.
    let alloc_err = |which: &'static str| {
        CodecError::Av1Encode(format!("subsample_420: {which} allocation failed"))
    };
    let mut cb_out: Vec<u16> = Vec::new();
    cb_out
        .try_reserve_exact(out_n)
        .map_err(|_| alloc_err("Cb plane"))?;
    let mut cr_out: Vec<u16> = Vec::new();
    cr_out
        .try_reserve_exact(out_n)
        .map_err(|_| alloc_err("Cr plane"))?;

    for row in 0..ch {
        for col in 0..cw {
            let r0 = row * 2;
            let c0 = col * 2;
            let i00 = r0 * width + c0;
            let i01 = i00 + 1;
            let i10 = (r0 + 1) * width + c0;
            let i11 = i10 + 1;

            let cb_avg = ((cb_plane[i00] as u32
                + cb_plane[i01] as u32
                + cb_plane[i10] as u32
                + cb_plane[i11] as u32
                + 2)
                / 4) as u16;
            let cr_avg = ((cr_plane[i00] as u32
                + cr_plane[i01] as u32
                + cr_plane[i10] as u32
                + cr_plane[i11] as u32
                + 2)
                / 4) as u16;
            cb_out.push(cb_avg);
            cr_out.push(cr_avg);
        }
    }

    Ok(Planes420 {
        y: y_plane.to_vec(),
        cb: cb_out,
        cr: cr_out,
    })
}

/// A pair of upsampled Cb / Cr planes returned by [`upsample_420`].
pub struct UpsampledChroma {
    /// Full-resolution Cb (U) plane.
    pub cb: Vec<u16>,
    /// Full-resolution Cr (V) plane.
    pub cr: Vec<u16>,
}

/// Input + output dimensions for the 4:2:0 → 4:4:4 chroma upsampler.
///
/// Grouping the four dimensions into a single argument keeps the public
/// `upsample_420` / `upsample_420_into` signatures compact and gives
/// callers an obvious place to add new geometry-related fields in the
/// future (e.g. sample-position offsets).
pub struct UpsampleDims {
    /// Subsampled plane width in samples (typically `(full_width + 1) / 2`).
    pub chroma_w: usize,
    /// Subsampled plane height in lines (typically `(full_height + 1) / 2`).
    pub chroma_h: usize,
    /// Full-resolution frame width in pixels.
    pub full_width: usize,
    /// Full-resolution frame height in lines.
    pub full_height: usize,
}

/// Upsamples Cb and Cr planes from 4:2:0 back to 4:4:4 using nearest-neighbour
/// replication.
///
/// Each 2×2 block of full-resolution pixels receives the same chroma value
/// from the corresponding subsampled sample. This is the simplest upsampling
/// method and introduces no additional filtering artefacts. For higher quality,
/// a bilinear or Lanczos filter could be substituted.
///
/// # Arguments
/// * `cb_plane` — Subsampled Cb (U) plane (`dims.chroma_w × dims.chroma_h` samples).
/// * `cr_plane` — Subsampled Cr (V) plane (`dims.chroma_w × dims.chroma_h` samples).
/// * `dims`     — Source + destination dimensions; see [`UpsampleDims`].
///
/// # Returns
/// Full-resolution Cb/Cr planes packaged as [`UpsampledChroma`].
pub fn upsample_420(
    cb_plane: &[u16],
    cr_plane: &[u16],
    dims: UpsampleDims,
) -> Result<UpsampledChroma, CodecError> {
    let mut cb_out = Vec::new();
    let mut cr_out = Vec::new();
    upsample_420_into(cb_plane, cr_plane, dims, &mut cb_out, &mut cr_out)?;
    Ok(UpsampledChroma {
        cb: cb_out,
        cr: cr_out,
    })
}

/// Buffer-reuse variant of [`upsample_420`] that writes into caller-provided
/// output vectors.
pub fn upsample_420_into(
    cb_plane: &[u16],
    cr_plane: &[u16],
    dims: UpsampleDims,
    cb_out: &mut Vec<u16>,
    cr_out: &mut Vec<u16>,
) -> Result<(), CodecError> {
    let UpsampleDims {
        chroma_w,
        chroma_h,
        full_width,
        full_height,
    } = dims;
    let cw = chroma_w;
    let ch = chroma_h;
    let chroma_samples = cw.checked_mul(ch).ok_or_else(|| {
        CodecError::Av1Decode("upsample_420: chroma_w × chroma_h overflows usize".into())
    })?;
    if cb_plane.len() != chroma_samples || cr_plane.len() != chroma_samples {
        return Err(CodecError::Av1Decode(format!(
            "upsample_420: expected {chroma_samples} chroma samples per plane, got cb={}, cr={}",
            cb_plane.len(),
            cr_plane.len()
        )));
    }

    let out_n = full_width.checked_mul(full_height).ok_or_else(|| {
        CodecError::Av1Decode("upsample_420: full width × height overflows usize".into())
    })?;
    cb_out.clear();
    cr_out.clear();
    // T-2: guard the resize-to-out_n via try_reserve_exact so an OOM at
    // very large plane sizes returns a recoverable error instead of
    // aborting the process. `out_n` is already bounded by checked_mul
    // above and (transitively) by the reader's frame-pixel cap.
    let alloc_err = |which: &'static str| {
        CodecError::Av1Decode(format!("upsample_420: {which} allocation failed"))
    };
    cb_out
        .try_reserve_exact(out_n)
        .map_err(|_| alloc_err("Cb output plane"))?;
    cr_out
        .try_reserve_exact(out_n)
        .map_err(|_| alloc_err("Cr output plane"))?;
    cb_out.resize(out_n, 0);
    cr_out.resize(out_n, 0);

    for row in 0..full_height {
        for col in 0..full_width {
            // Map full-resolution coordinates to subsampled coordinates,
            // clamping to the valid range of the subsampled plane.
            let sub_row = (row / 2).min(ch.saturating_sub(1));
            let sub_col = (col / 2).min(cw.saturating_sub(1));
            let sub_idx = sub_row
                .checked_mul(cw)
                .and_then(|b| b.checked_add(sub_col))
                .ok_or_else(|| {
                    CodecError::Av1Decode("upsample_420: chroma index overflow".into())
                })?;

            let o = row
                .checked_mul(full_width)
                .and_then(|b| b.checked_add(col))
                .ok_or_else(|| {
                    CodecError::Av1Decode("upsample_420: output index overflow".into())
                })?;
            cb_out[o] = cb_plane[sub_idx];
            cr_out[o] = cr_plane[sub_idx];
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// AV1 bit depth used for QDRV delivery-tier encoding.
/// 12-bit provides 4096 levels per channel, sufficient to represent the
/// SMPTE ST 2084 PQ signal with no perceptible quantisation artefacts.
const AV1_BIT_DEPTH: usize = 12;

/// Maximum sample value for 12-bit encoding: 2^12 − 1 = 4095.
const MAX_SAMPLE: f32 = 4095.0;

// ---------------------------------------------------------------------------
// Public encode function
// ---------------------------------------------------------------------------

/// Encodes one QDRV delivery-tier frame to a self-contained AV1 bitstream.
///
/// The output is a complete AV1 still-picture bitstream (sequence header OBU
/// followed by a frame OBU) that can be decoded independently by any
/// AV1-compliant decoder. It is intended to be stored as the pixel data
/// payload of a QDRV container frame block.
///
/// # Arguments
/// * `pixels` — Delivery-tier pixel buffer. Length must equal `width × height`.
/// * `width`  — Frame width in pixels.
/// * `height` — Frame height in pixels.
/// * `config` — AV1 encoder configuration.
///
/// # Errors
/// Returns [`CodecError::Av1Encode`] if rav1e rejects the configuration
/// or encounters a fatal error.  
/// Returns [`CodecError::NoPacketsProduced`] if the encoder produces no
/// output (should not occur in normal operation).
pub fn encode_frame(
    pixels: &[Pixel32],
    width: u32,
    height: u32,
    config: &Av1Config,
) -> Result<Vec<u8>, CodecError> {
    config.validate()?;

    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return Err(CodecError::Av1Encode(
            "frame width and height must both be greater than zero".into(),
        ));
    }

    if matches!(config.chroma, ChromaSampling420::Cs420)
        && (!w.is_multiple_of(2) || !h.is_multiple_of(2))
    {
        return Err(CodecError::Av1Encode(
            "4:2:0 AV1 encoding requires even width and height".into(),
        ));
    }

    let pixel_count = w
        .checked_mul(h)
        .ok_or_else(|| CodecError::Av1Encode("width × height overflows usize".into()))?;
    if pixels.len() != pixel_count {
        return Err(CodecError::Av1Encode(format!(
            "pixel buffer length {} does not match width × height ({pixel_count})",
            pixels.len()
        )));
    }

    // ---------------------------------------------------------------------------
    // Step 1 + 2: Convert Float32 RGB → 12-bit YCbCr u16 for all three planes.
    // try_reserve_exact so a maliciously-large but in-bound frame returns a
    // graceful error rather than panicking on OOM.
    // ---------------------------------------------------------------------------
    let alloc_err =
        |which: &'static str| CodecError::Av1Encode(format!("{which} allocation failed"));
    let mut y_plane: Vec<u16> = Vec::new();
    y_plane
        .try_reserve_exact(pixel_count)
        .map_err(|_| alloc_err("Y plane"))?;
    let mut cb_plane: Vec<u16> = Vec::new();
    cb_plane
        .try_reserve_exact(pixel_count)
        .map_err(|_| alloc_err("Cb plane"))?;
    let mut cr_plane: Vec<u16> = Vec::new();
    cr_plane
        .try_reserve_exact(pixel_count)
        .map_err(|_| alloc_err("Cr plane"))?;

    for p in pixels {
        let ycbcr = YCbCr32::from(*p);

        // Y: [0.0, 1.0] → [0, 4095]
        let y_u16 = (ycbcr.y.clamp(0.0, 1.0) * MAX_SAMPLE).round() as u16;
        // Cb: [-0.5, 0.5] → [0, 4095] via offset by 0.5
        let cb_u16 = ((ycbcr.cb + 0.5).clamp(0.0, 1.0) * MAX_SAMPLE).round() as u16;
        // Cr: [-0.5, 0.5] → [0, 4095] via offset by 0.5
        let cr_u16 = ((ycbcr.cr + 0.5).clamp(0.0, 1.0) * MAX_SAMPLE).round() as u16;

        y_plane.push(y_u16);
        cb_plane.push(cb_u16);
        cr_plane.push(cr_u16);
    }

    // ---------------------------------------------------------------------------
    // Step 3: Configure rav1e and build the encoder context.
    // ---------------------------------------------------------------------------
    let effective_quantizer = if config.lossless { 0 } else { config.quantizer };

    let mut enc = EncoderConfig::with_speed_preset(config.speed);
    enc.width = w;
    enc.height = h;
    enc.bit_depth = AV1_BIT_DEPTH;
    enc.chroma_sampling = match config.chroma {
        ChromaSampling420::Cs444 => ChromaSampling::Cs444,
        ChromaSampling420::Cs420 => ChromaSampling::Cs420,
    };
    enc.pixel_range = PixelRange::Full;
    enc.still_picture = true;
    enc.low_latency = true;
    enc.quantizer = effective_quantizer;
    enc.min_key_frame_interval = 0;
    enc.max_key_frame_interval = 1;

    // Signal ITU-R Rec. 2100 HDR colour characteristics in the AV1 bitstream
    // so that any downstream decoder can identify this as PQ HDR content.
    enc.color_description = Some(ColorDescription {
        color_primaries: ColorPrimaries::BT2020,
        transfer_characteristics: TransferCharacteristics::SMPTE2084,
        matrix_coefficients: MatrixCoefficients::BT2020NCL,
    });

    let cfg = Config::new()
        .with_encoder_config(enc)
        .with_threads(config.threads);

    let mut ctx: Context<u16> = cfg
        .new_context()
        .map_err(|e| CodecError::Av1Encode(format!("invalid encoder config: {e:?}")))?;

    // ---------------------------------------------------------------------------
    // Step 3b: If 4:2:0, downsample the chroma planes before filling rav1e.
    // ---------------------------------------------------------------------------
    let (final_y, final_cb, final_cr, chroma_w) = match config.chroma {
        ChromaSampling420::Cs420 => {
            let planes = subsample_420(&y_plane, &cb_plane, &cr_plane, w, h)?;
            (planes.y, planes.cb, planes.cr, w / 2)
        }
        ChromaSampling420::Cs444 => (y_plane, cb_plane, cr_plane, w),
    };

    // ---------------------------------------------------------------------------
    // Step 4: Fill the rav1e frame planes and send to the encoder.
    // ---------------------------------------------------------------------------
    let mut frame = ctx.new_frame();
    fill_plane(&mut frame.planes[0], w, &final_y);
    fill_plane(&mut frame.planes[1], chroma_w, &final_cb);
    fill_plane(&mut frame.planes[2], chroma_w, &final_cr);

    ctx.send_frame(frame)
        .map_err(|e| CodecError::Av1Encode(format!("send_frame failed: {e:?}")))?;
    ctx.flush();

    // ---------------------------------------------------------------------------
    // Step 5: Drain all output packets and concatenate into one bitstream blob.
    // ---------------------------------------------------------------------------
    let mut av1_data = Vec::new();
    const MAX_IDLE_POLLS: usize = 10_000;
    let mut idle_polls = 0usize;

    loop {
        match ctx.receive_packet() {
            Ok(pkt) => {
                av1_data.extend_from_slice(&pkt.data);
                idle_polls = 0;
            }
            Err(EncoderStatus::LimitReached) => break,
            Err(EncoderStatus::Encoded) | Err(EncoderStatus::NeedMoreData) => {
                // The encoder processed the frame but has not yet emitted a
                // packet. This can occur with lookahead; continue draining.
                idle_polls += 1;
                if idle_polls > MAX_IDLE_POLLS {
                    return Err(CodecError::Av1Encode(format!(
                        "receive_packet stayed in Encoded/NeedMoreData for {MAX_IDLE_POLLS} polls"
                    )));
                }
                continue;
            }
            Err(e) => {
                return Err(CodecError::Av1Encode(format!(
                    "receive_packet failed: {e:?}"
                )));
            }
        }
    }

    if av1_data.is_empty() {
        return Err(CodecError::NoPacketsProduced(0));
    }

    Ok(av1_data)
}

// ---------------------------------------------------------------------------
// Public decode function
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DecodeScratch {
    y_vals: Vec<u16>,
    cb_vals: Vec<u16>,
    cr_vals: Vec<u16>,
    cb_subsampled: Vec<u16>,
    cr_subsampled: Vec<u16>,
}

/// Stateful AV1 decoder that reuses dav1d state and scratch buffers across
/// repeated frame decodes.
pub struct Av1Decoder {
    decoder: Decoder,
    scratch: DecodeScratch,
}

impl Av1Decoder {
    /// Creates a reusable AV1 decoder.
    pub fn new() -> Result<Self, CodecError> {
        let settings = Settings::default();
        let decoder = Decoder::with_settings(&settings)
            .map_err(|e| CodecError::Av1Decode(format!("decoder creation failed: {e:?}")))?;
        Ok(Self {
            decoder,
            scratch: DecodeScratch::default(),
        })
    }

    /// Decodes one AV1 still-picture frame with a reusable decoder instance.
    pub fn decode_frame(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<Pixel32>, CodecError> {
        // Drain any previously-ready pictures so each call reports output only
        // for the supplied bitstream.
        loop {
            match self.decoder.get_picture() {
                Ok(_) => {}
                Err(dav1d::Error::Again) => break,
                Err(e) => {
                    return Err(CodecError::Av1Decode(format!(
                        "get_picture before send_data failed: {e:?}"
                    )));
                }
            }
        }

        let mut first_picture: Option<dav1d::Picture> = None;
        // The dav1d-rs `send_data` API takes the bitstream by value, so we
        // pay one allocation per frame to clone our borrowed `&[u8]` into a
        // `Vec<u8>`. With the reader's 512 MB per-frame budget this can be
        // a meaningful memory pressure at extreme sizes; if upstream
        // dav1d-rs ever exposes a zero-copy variant, prefer it here.
        match self.decoder.send_data(data.to_vec(), None, None, None) {
            Ok(()) => {}
            Err(dav1d::Error::Again) => {
                // dav1d may keep part of the packet as pending input. While pending,
                // feed it via `send_pending_data` and interleave `get_picture` so
                // decoder output can unblock further input consumption.
                loop {
                    match self.decoder.get_picture() {
                        Ok(pic) => {
                            if first_picture.is_none() {
                                first_picture = Some(pic);
                            }
                        }
                        Err(dav1d::Error::Again) => {}
                        Err(e) => {
                            return Err(CodecError::Av1Decode(format!(
                                "get_picture during pending-data drain failed: {e:?}"
                            )));
                        }
                    }

                    match self.decoder.send_pending_data() {
                        Ok(()) => break,
                        Err(dav1d::Error::Again) => continue,
                        Err(e) => {
                            return Err(CodecError::Av1Decode(format!(
                                "send_pending_data failed: {e:?}"
                            )));
                        }
                    }
                }
            }
            Err(e) => {
                return Err(CodecError::Av1Decode(format!("send_data failed: {e:?}")));
            }
        }

        // Drain all output pictures. A still-picture OBU sequence should yield at
        // least one surface; additional pictures are ignored after the first.
        //
        // dav1d may transiently return `Again` before the first decoded picture is
        // ready (worker threads still processing). Poll a bounded number of initial
        // `Again` results before concluding no picture is available.
        const MAX_INITIAL_AGAIN_POLLS: usize = 32;
        let mut initial_again_polls = 0usize;
        loop {
            match self.decoder.get_picture() {
                Ok(pic) => {
                    if first_picture.is_none() {
                        first_picture = Some(pic);
                    }
                    initial_again_polls = 0;
                }
                Err(dav1d::Error::Again) => {
                    if first_picture.is_none() && initial_again_polls < MAX_INITIAL_AGAIN_POLLS {
                        initial_again_polls += 1;
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    return Err(CodecError::Av1Decode(format!("get_picture failed: {e:?}")));
                }
            }
        }

        let picture = first_picture.ok_or(CodecError::NoPictureDecoded)?;

        // ---------------------------------------------------------------------------
        // Validate picture dimensions.
        // ---------------------------------------------------------------------------
        let actual_w = picture.width();
        let actual_h = picture.height();
        if actual_w != width || actual_h != height {
            return Err(CodecError::PictureDimensionMismatch {
                expected_w: width,
                expected_h: height,
                actual_w,
                actual_h,
            });
        }

        let w = width as usize;
        let h = height as usize;

        let pixel_count = w
            .checked_mul(h)
            .ok_or_else(|| CodecError::Av1Decode("width × height overflows usize".into()))?;

        // ---------------------------------------------------------------------------
        // Extract and normalise decoded YCbCr planes.
        // ---------------------------------------------------------------------------
        let layout = picture.pixel_layout();
        extract_plane_u16_into(
            &picture,
            PlanarImageComponent::Y,
            w,
            h,
            &mut self.scratch.y_vals,
        )?;

        match layout {
            PixelLayout::I444 => {
                extract_plane_u16_into(
                    &picture,
                    PlanarImageComponent::U,
                    w,
                    h,
                    &mut self.scratch.cb_vals,
                )?;
                extract_plane_u16_into(
                    &picture,
                    PlanarImageComponent::V,
                    w,
                    h,
                    &mut self.scratch.cr_vals,
                )?;
            }
            PixelLayout::I420 => {
                let cw = w.div_ceil(2);
                let ch = h.div_ceil(2);
                extract_plane_u16_into(
                    &picture,
                    PlanarImageComponent::U,
                    cw,
                    ch,
                    &mut self.scratch.cb_subsampled,
                )?;
                extract_plane_u16_into(
                    &picture,
                    PlanarImageComponent::V,
                    cw,
                    ch,
                    &mut self.scratch.cr_subsampled,
                )?;
                upsample_420_into(
                    &self.scratch.cb_subsampled,
                    &self.scratch.cr_subsampled,
                    UpsampleDims {
                        chroma_w: cw,
                        chroma_h: ch,
                        full_width: w,
                        full_height: h,
                    },
                    &mut self.scratch.cb_vals,
                    &mut self.scratch.cr_vals,
                )?;
            }
            PixelLayout::I422 | PixelLayout::I400 => {
                return Err(CodecError::Av1Decode(format!(
                    "unsupported chroma layout for QDRV delivery decode: {layout:?}"
                )));
            }
        };

        // ---------------------------------------------------------------------------
        // Dequantise u16 → Float32 YCbCr, then convert to RGB Pixel32.
        // try_reserve_exact: pixel_count comes from caller-validated dims,
        // but a graceful AllocationFailed-style error beats panicking on OOM.
        // ---------------------------------------------------------------------------
        let mut pixels: Vec<Pixel32> = Vec::new();
        pixels.try_reserve_exact(pixel_count).map_err(|_| {
            CodecError::Av1Decode(format!(
                "pixel buffer allocation failed for {pixel_count} pixels"
            ))
        })?;
        for i in 0..pixel_count {
            // Dequantise Y: [0, 4095] → [0.0, 1.0]
            let y = self.scratch.y_vals[i] as f32 / MAX_SAMPLE;
            // Dequantise Cb: [0, 4095] → [-0.5, 0.5]
            let cb = self.scratch.cb_vals[i] as f32 / MAX_SAMPLE - 0.5;
            // Dequantise Cr: [0, 4095] → [-0.5, 0.5]
            let cr = self.scratch.cr_vals[i] as f32 / MAX_SAMPLE - 0.5;

            let ycbcr = YCbCr32 { y, cb, cr };
            pixels.push(ycbcr.to_rgb());
        }

        Ok(pixels)
    }
}

/// Decodes a self-contained AV1 bitstream produced by [`encode_frame`] back
/// to QDRV delivery-tier pixels.
///
/// # Arguments
/// * `data`   — Complete AV1 still-picture bitstream bytes.
/// * `width`  — Expected frame width in pixels.
/// * `height` — Expected frame height in pixels.
///
/// # Errors
/// Returns [`CodecError::Av1Decode`] if dav1d reports a decoding error.  
/// Returns [`CodecError::NoPictureDecoded`] if dav1d produces no output.  
/// Returns [`CodecError::PictureDimensionMismatch`] if the decoded picture
/// dimensions differ from the expected `width × height`.
pub fn decode_frame(data: &[u8], width: u32, height: u32) -> Result<Vec<Pixel32>, CodecError> {
    let mut decoder = Av1Decoder::new()?;
    decoder.decode_frame(data, width, height)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Fills a rav1e `Plane<u16>` from a flat row-major slice of u16 values.
///
/// rav1e planes may have a stride larger than the active width due to memory
/// alignment. This function accounts for the stride by writing row by row,
/// leaving padding samples at the end of each row undisturbed.
///
/// Exposed `pub(crate)` so the temporal encoder can share the same
/// implementation instead of carrying a verbatim duplicate.
pub(crate) fn fill_plane(plane: &mut Plane<u16>, width: usize, values: &[u16]) {
    if width == 0 {
        debug_assert!(
            values.is_empty(),
            "fill_plane received width=0 with {} values",
            values.len()
        );
        return;
    }
    let stride = plane.cfg.stride;
    let data = plane.data_origin_mut();
    if stride == 0 {
        debug_assert!(
            values.is_empty(),
            "fill_plane received stride=0 with {} values",
            values.len()
        );
        return;
    }
    let max_rows = data.len() / stride;
    let required_rows = values.len().div_ceil(width);
    debug_assert!(
        required_rows <= max_rows,
        "fill_plane source rows ({required_rows}) exceed plane rows ({max_rows})"
    );

    for (row_idx, row_values) in values.chunks(width).take(max_rows).enumerate() {
        let row_start = row_idx * stride;
        for (col, &val) in row_values.iter().take(stride).enumerate() {
            data[row_start + col] = val;
        }
    }
}

/// Extracts one plane from a dav1d `Picture` into a caller-provided
/// row-major `Vec<u16>`.
///
/// dav1d stores 12-bit sample values as **host-endian** `u16` words inside a
/// byte slice: on little-endian targets the bytes are in LE order, on
/// big-endian targets they are in BE order, in both cases matching the
/// platform's native `u16` representation. Using `u16::from_ne_bytes` here
/// is therefore correct on both, even though many file formats fix an
/// endianness on disk — this is an in-memory C struct exposed as bytes, not
/// a byte stream.
///
/// [`Picture::stride`](dav1d::Picture::stride) is the **byte** stride between
/// consecutive rows for that plane (the `dav1d` crate’s doc string says
/// “pixels”, but the value is used here as a byte offset, matching dav1d’s C
/// layout and the crate’s own tests).
///
/// Returns an error instead of panicking if the plane buffer is shorter than
/// the active `width × height` region implied by `stride` (truncated or corrupt
/// bitstream).
fn extract_plane_u16_into(
    picture: &dav1d::Picture,
    component: PlanarImageComponent,
    width: usize,
    height: usize,
    values: &mut Vec<u16>,
) -> Result<(), CodecError> {
    let plane = picture.plane(component);
    let plane_bytes: &[u8] = plane.as_ref();
    let stride = usize::try_from(picture.stride(component))
        .map_err(|_| CodecError::Av1Decode("decoded plane stride conversion failed".into()))?;

    let row_bytes_len = width
        .checked_mul(2)
        .ok_or_else(|| CodecError::Av1Decode("decoded plane active width overflow".into()))?;

    let values_len = width
        .checked_mul(height)
        .ok_or_else(|| CodecError::Av1Decode("decoded plane sample count overflow".into()))?;
    values.clear();
    // T-4: prefer try_reserve_exact for consistency with the rest of the
    // codec's allocation pattern. The plane size has already passed
    // `width * height` overflow + dav1d's own size validation, so the
    // request is bounded against the reader's frame-pixel limits.
    values.try_reserve_exact(values_len).map_err(|_| {
        CodecError::Av1Decode(format!(
            "decoded plane buffer allocation failed for {values_len} samples"
        ))
    })?;

    for row in 0..height {
        let row_start = row
            .checked_mul(stride)
            .ok_or_else(|| CodecError::Av1Decode("decoded plane row offset overflow".into()))?;
        let row_end = row_start
            .checked_add(row_bytes_len)
            .ok_or_else(|| CodecError::Av1Decode("decoded plane row byte range overflow".into()))?;
        let row_bytes = plane_bytes.get(row_start..row_end).ok_or_else(|| {
            CodecError::Av1Decode(format!(
                "decoded plane buffer too short for row {row} (component {component:?}, \
                 stride_bytes={stride}, need {row_end} bytes, have {})",
                plane_bytes.len()
            ))
        })?;
        for chunk in row_bytes.chunks_exact(2) {
            let val = u16::from_ne_bytes([chunk[0], chunk[1]]);
            values.push(val);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_av1_config_validate_rejects_out_of_range_speed() {
        let cfg = Av1Config {
            speed: 11,
            ..Av1Config::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(format!("{err}").contains("speed"));
    }

    #[test]
    fn test_av1_config_validate_rejects_out_of_range_quantizer() {
        let cfg = Av1Config {
            quantizer: 256,
            ..Av1Config::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(format!("{err}").contains("quantizer"));
    }

    #[test]
    fn test_av1_config_validate_rejects_invalid_lossless_pairing() {
        let cfg = Av1Config {
            lossless: true,
            quantizer: 1,
            ..Av1Config::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(format!("{err}").contains("lossless=true requires quantizer=0"));
    }

    #[test]
    fn test_encode_frame_rejects_zero_dimensions() {
        let cfg = Av1Config::default();
        let err = encode_frame(&[], 0, 1, &cfg).unwrap_err();
        assert!(format!("{err}").contains("greater than zero"));
    }

    #[test]
    fn test_encode_decode_roundtrip_smoke() {
        let cfg = Av1Config {
            quantizer: 0,
            lossless: true,
            ..Av1Config::default()
        };
        let w = 8u32;
        let h = 4u32;
        let pixels = vec![Pixel32::new_unchecked(0.25, 0.5, 0.75); (w * h) as usize];

        let encoded = encode_frame(&pixels, w, h, &cfg).expect("encode must succeed");
        assert!(
            !encoded.is_empty(),
            "encode produced empty bitstream for a valid frame"
        );
        let decoded = decode_frame(&encoded, w, h).expect("decode must succeed");
        assert_eq!(decoded.len(), pixels.len());
    }

    #[test]
    fn test_upsample_420_into_matches_allocating_variant() {
        let cb_sub = vec![10u16, 20, 30, 40];
        let cr_sub = vec![100u16, 110, 120, 130];

        let dims = UpsampleDims {
            chroma_w: 2,
            chroma_h: 2,
            full_width: 4,
            full_height: 4,
        };
        let expected = upsample_420(&cb_sub, &cr_sub, dims).unwrap();
        let mut cb_out = vec![999u16; 3];
        let mut cr_out = vec![888u16; 5];
        upsample_420_into(
            &cb_sub,
            &cr_sub,
            UpsampleDims {
                chroma_w: 2,
                chroma_h: 2,
                full_width: 4,
                full_height: 4,
            },
            &mut cb_out,
            &mut cr_out,
        )
        .unwrap();
        assert_eq!(cb_out, expected.cb);
        assert_eq!(cr_out, expected.cr);

        // Reuse the same output buffers with a different size and verify stale
        // values are replaced.
        upsample_420_into(
            &[7u16],
            &[9u16],
            UpsampleDims {
                chroma_w: 1,
                chroma_h: 1,
                full_width: 2,
                full_height: 2,
            },
            &mut cb_out,
            &mut cr_out,
        )
        .unwrap();
        assert_eq!(cb_out, vec![7u16; 4]);
        assert_eq!(cr_out, vec![9u16; 4]);
    }

    #[test]
    fn test_reusable_decoder_matches_one_shot_for_repeated_decodes() {
        let cfg = Av1Config {
            quantizer: 0,
            lossless: true,
            ..Av1Config::default()
        };
        let w = 8u32;
        let h = 4u32;

        let pixels_a = vec![Pixel32::new_unchecked(0.10, 0.20, 0.30); (w * h) as usize];
        let pixels_b = vec![Pixel32::new_unchecked(0.70, 0.15, 0.55); (w * h) as usize];
        let encoded_a = encode_frame(&pixels_a, w, h, &cfg).expect("encode A must succeed");
        let encoded_b = encode_frame(&pixels_b, w, h, &cfg).expect("encode B must succeed");

        let expected_a = decode_frame(&encoded_a, w, h).expect("one-shot decode A must succeed");
        let expected_b = decode_frame(&encoded_b, w, h).expect("one-shot decode B must succeed");

        let mut reusable = Av1Decoder::new().expect("reusable decoder creation must succeed");
        let decoded_a = reusable
            .decode_frame(&encoded_a, w, h)
            .expect("reusable decode A must succeed");
        let decoded_b = reusable
            .decode_frame(&encoded_b, w, h)
            .expect("reusable decode B must succeed");

        assert_eq!(decoded_a, expected_a);
        assert_eq!(decoded_b, expected_b);
    }
}
