// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Temporal AV1 encoding for multi-frame QDRV delivery streams.
//!
//! Provides a stateful encoder that accepts delivery-tier frames and produces
//! a temporally-compressed AV1 bitstream with GOP (Group of Pictures) support.
//! Unlike [`crate::av1_encode`], which encodes each frame as an independent
//! still picture, `TemporalEncoder` uses inter-frame prediction for
//! significantly improved compression on video content.
//!
//! ## GOP structure
//!
//! The encoder uses a simple keyframe-interval model: every `keyframe_interval`
//! frames, a keyframe (random access point) is emitted. All other frames are
//! inter-predicted. rav1e handles the low-level reference frame management.

use qdrv_core::pixel::{Pixel32, YCbCr32};
use rav1e::prelude::*;

use crate::av1::{Av1Config, ChromaSampling420, fill_plane};
use crate::error::CodecError;

const AV1_BIT_DEPTH: usize = 12;
const MAX_SAMPLE: f32 = 4095.0;

/// Configuration for temporal (multi-frame) AV1 encoding.
#[derive(Debug, Clone)]
pub struct GopConfig {
    /// Maximum number of frames between keyframes. A keyframe interval of 1
    /// produces all-intra encoding (equivalent to the current still-picture
    /// mode). Typical values: 48–250.
    pub keyframe_interval: u32,

    /// Maximum number of consecutive B-frames between reference frames.
    /// Set to 0 to disable B-frames entirely.
    pub max_b_frames: u32,
}

impl Default for GopConfig {
    fn default() -> Self {
        Self {
            keyframe_interval: 120,
            max_b_frames: 0,
        }
    }
}

/// An encoded AV1 packet produced by the temporal encoder.
#[derive(Debug)]
pub struct EncodedPacket {
    /// The encoded AV1 OBU bytes.
    pub data: Vec<u8>,
    /// Zero-based presentation frame index.
    pub frame_index: u64,
    /// `true` if this packet is a keyframe (random access point).
    pub is_keyframe: bool,
}

/// A stateful temporal AV1 encoder that accepts frames and emits packets.
///
/// Unlike [`crate::av1_encode`], which encodes each frame independently as a
/// still picture, `TemporalEncoder` maintains inter-frame state for temporal
/// prediction, producing significantly smaller bitstreams for video content.
pub struct TemporalEncoder {
    ctx: Context<u16>,
    width: usize,
    height: usize,
    chroma: ChromaSampling420,
    frame_count: u64,
    flushed: bool,
}

impl TemporalEncoder {
    /// Creates a new temporal encoder with the given configuration.
    ///
    /// # Errors
    /// Returns [`CodecError::Av1Encode`] if rav1e rejects the configuration.
    pub fn new(
        width: u32,
        height: u32,
        av1_config: &Av1Config,
        gop_config: &GopConfig,
    ) -> Result<Self, CodecError> {
        av1_config.validate()?;

        let w = width as usize;
        let h = height as usize;
        if w == 0 || h == 0 {
            return Err(CodecError::Av1Encode(
                "temporal encoder width and height must both be greater than zero".into(),
            ));
        }

        if matches!(av1_config.chroma, ChromaSampling420::Cs420)
            && (!w.is_multiple_of(2) || !h.is_multiple_of(2))
        {
            return Err(CodecError::Av1Encode(
                "4:2:0 temporal encoding requires even width and height".into(),
            ));
        }

        let effective_q = if av1_config.lossless {
            0
        } else {
            av1_config.quantizer
        };

        let mut enc = EncoderConfig::with_speed_preset(av1_config.speed);
        enc.width = w;
        enc.height = h;
        enc.bit_depth = AV1_BIT_DEPTH;
        enc.chroma_sampling = match av1_config.chroma {
            ChromaSampling420::Cs444 => ChromaSampling::Cs444,
            ChromaSampling420::Cs420 => ChromaSampling::Cs420,
        };
        enc.pixel_range = PixelRange::Full;
        enc.still_picture = false;
        enc.low_latency = gop_config.max_b_frames == 0;
        enc.quantizer = effective_q;
        enc.min_key_frame_interval = 1;
        enc.max_key_frame_interval = gop_config.keyframe_interval as u64;

        enc.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT2020,
            transfer_characteristics: TransferCharacteristics::SMPTE2084,
            matrix_coefficients: MatrixCoefficients::BT2020NCL,
        });

        let cfg = Config::new()
            .with_encoder_config(enc)
            .with_threads(av1_config.threads);

        let ctx: Context<u16> = cfg.new_context().map_err(|e| {
            CodecError::Av1Encode(format!("invalid temporal encoder config: {e:?}"))
        })?;

        Ok(Self {
            ctx,
            width: w,
            height: h,
            chroma: av1_config.chroma,
            frame_count: 0,
            flushed: false,
        })
    }

    /// Sends a delivery-tier frame to the encoder.
    ///
    /// The frame is converted to 12-bit YCbCr and queued for temporal encoding.
    /// Call [`receive_packets`](Self::receive_packets) after each send to
    /// retrieve any packets the encoder has produced.
    pub fn send_frame(&mut self, pixels: &[Pixel32]) -> Result<(), CodecError> {
        if self.flushed {
            return Err(CodecError::Av1Encode("encoder already flushed".to_string()));
        }

        let pixel_count = self.width.checked_mul(self.height).ok_or_else(|| {
            CodecError::Av1Encode("encoder width × height overflows usize".into())
        })?;
        if pixels.len() != pixel_count {
            return Err(CodecError::Av1Encode(format!(
                "expected {} pixels, got {}",
                pixel_count,
                pixels.len()
            )));
        }

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
            y_plane.push((ycbcr.y.clamp(0.0, 1.0) * MAX_SAMPLE).round() as u16);
            cb_plane.push(((ycbcr.cb + 0.5).clamp(0.0, 1.0) * MAX_SAMPLE).round() as u16);
            cr_plane.push(((ycbcr.cr + 0.5).clamp(0.0, 1.0) * MAX_SAMPLE).round() as u16);
        }

        // If 4:2:0, downsample chroma planes before filling rav1e.
        let (final_y, final_cb, final_cr, chroma_w) = match self.chroma {
            ChromaSampling420::Cs420 => {
                let planes = crate::av1::subsample_420(
                    &y_plane,
                    &cb_plane,
                    &cr_plane,
                    self.width,
                    self.height,
                )?;
                (planes.y, planes.cb, planes.cr, self.width / 2)
            }
            ChromaSampling420::Cs444 => (y_plane, cb_plane, cr_plane, self.width),
        };

        let mut frame = self.ctx.new_frame();
        fill_plane(&mut frame.planes[0], self.width, &final_y);
        fill_plane(&mut frame.planes[1], chroma_w, &final_cb);
        fill_plane(&mut frame.planes[2], chroma_w, &final_cr);

        self.ctx
            .send_frame(frame)
            .map_err(|e| CodecError::Av1Encode(format!("send_frame failed: {e:?}")))?;

        self.frame_count += 1;
        Ok(())
    }

    /// Retrieves any encoded packets that are ready.
    ///
    /// Returns an empty `Vec` if the encoder has not yet accumulated enough
    /// frames to emit a packet.
    pub fn receive_packets(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        let mut packets = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    packets.push(EncodedPacket {
                        data: pkt.data.to_vec(),
                        frame_index: pkt.input_frameno,
                        is_keyframe: pkt.frame_type == FrameType::KEY,
                    });
                }
                Err(EncoderStatus::LimitReached) => break,
                // `Encoded` / `NeedMoreData` mean rav1e has no further packets ready *right
                // now* for this non-draining poll. Breaking returns whatever has been
                // produced so far; the caller may `send_frame` again and poll later.
                Err(EncoderStatus::Encoded) | Err(EncoderStatus::NeedMoreData) => break,
                Err(e) => {
                    return Err(CodecError::Av1Encode(format!(
                        "receive_packet failed: {e:?}"
                    )));
                }
            }
        }
        Ok(packets)
    }

    /// Flushes all remaining frames through the encoder.
    ///
    /// Must be called after the last frame has been sent. Returns all
    /// remaining encoded packets. The encoder cannot accept more frames
    /// after flushing.
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>, CodecError> {
        if self.flushed {
            return Ok(Vec::new());
        }
        self.flushed = true;
        self.ctx.flush();

        let mut packets = Vec::new();
        const MAX_IDLE_POLLS: usize = 10_000;
        let mut idle_polls = 0usize;
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    packets.push(EncodedPacket {
                        data: pkt.data.to_vec(),
                        frame_index: pkt.input_frameno,
                        is_keyframe: pkt.frame_type == FrameType::KEY,
                    });
                    idle_polls = 0;
                }
                Err(EncoderStatus::LimitReached) => break,
                // During flush we must drain the encoder until `LimitReached`. `Encoded` /
                // `NeedMoreData` are benign back-pressure signals while rav1e catches up,
                // so we keep polling instead of returning early (contrast with
                // `receive_packets`, which intentionally stops after one non-terminal pass).
                Err(EncoderStatus::Encoded) | Err(EncoderStatus::NeedMoreData) => {
                    idle_polls += 1;
                    if idle_polls > MAX_IDLE_POLLS {
                        return Err(CodecError::Av1Encode(format!(
                            "flush receive_packet stayed in Encoded/NeedMoreData for {MAX_IDLE_POLLS} polls"
                        )));
                    }
                    continue;
                }
                Err(e) => {
                    return Err(CodecError::Av1Encode(format!(
                        "flush receive_packet failed: {e:?}"
                    )));
                }
            }
        }
        Ok(packets)
    }

    /// Returns the number of frames sent so far.
    pub fn frames_sent(&self) -> u64 {
        self.frame_count
    }
}

// `fill_plane` is shared with the still-picture encoder in `av1.rs`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temporal_new_rejects_zero_dimensions() {
        let av1_cfg = Av1Config::default();
        let gop_cfg = GopConfig::default();
        let err = match TemporalEncoder::new(0, 1, &av1_cfg, &gop_cfg) {
            Ok(_) => panic!("expected zero-dimension constructor to fail"),
            Err(err) => err,
        };
        assert!(format!("{err}").contains("greater than zero"));
    }
}
