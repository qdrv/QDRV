// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! AV1 elementary-stream writers for developer and tooling workflows.
//!
//! These are deliberately minimal "containers" aimed at the people who *make*
//! the video — codec engineers, conformance and bitstream analysts — rather
//! than consumer playback:
//!
//! | Writer | Output | Purpose |
//! |--------|--------|---------|
//! | [`write_ivf`] | `.ivf` | The trivial On2/AOM test container. Pipes QDRV delivery frames straight into the AV1 reference tooling (`aomdec`, `dav1d` test harnesses, IVF-aware analysers). |
//! | [`write_obu_stream`] | `.obu` | The bare AV1 low-overhead OBU elementary stream — no container at all, just the temporal units back to back, for bitstream inspection. |
//!
//! Both consume the same [`MuxFrame`] slice the ISOBMFF muxer does, so a single
//! decode/encode pass can fan out to every delivery target.

use std::io::Write;

use crate::{Mp4Config, MuxError, MuxFrame, usize_to_u32};

/// IVF timebase denominator. Frame timestamps are emitted as plain frame
/// indices, and the header's `rate`/`scale` pair is chosen so a player
/// recovers `time = index / frame_rate` seconds. Using a fixed 1000-unit
/// scale keeps the rate numerator an exact integer for the common integer and
/// NTSC-family frame rates without resorting to floating-point timestamps.
const IVF_TIMEBASE_SCALE: u32 = 1000;

/// Writes the AV1 frames as an IVF (`.ivf`) elementary-stream file.
///
/// IVF is the minimal On2/AOM test container: a 32-byte file header followed,
/// for each frame, by a 12-byte frame header (`size`, `timestamp`) and the raw
/// AV1 temporal-unit bytes. IVF itself defines no metadata fields, but because
/// QDRV carries its HDR signalling and dynamic metadata *inside* the AV1
/// bitstream (sequence-header colour description plus ITU-T T.35 metadata OBUs),
/// the per-frame bytes written here retain both.
///
/// # Errors
/// - [`MuxError::Io`] — the output writer failed.
/// - [`MuxError::SizeOverflow`] — a frame, the frame count, or the derived
///   frame-rate numerator does not fit the IVF 32-bit fields, or the
///   configured dimensions exceed the IVF 16-bit width/height fields.
pub fn write_ivf<W: Write>(
    writer: &mut W,
    config: &Mp4Config,
    frames: &[MuxFrame],
) -> Result<(), MuxError> {
    if frames.is_empty() {
        return Err(MuxError::NoFrames);
    }
    if !config.frame_rate.is_finite() || config.frame_rate <= 0.0 {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "frame_rate must be positive and finite, got {}",
                config.frame_rate
            ),
        )));
    }
    if config.width == 0
        || config.height == 0
        || config.width > u16::MAX as u32
        || config.height > u16::MAX as u32
    {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "IVF width/height must be in 1..={} (got {}x{})",
                u16::MAX,
                config.width,
                config.height
            ),
        )));
    }

    // rate / scale chosen so per-frame timestamps are plain frame indices and a
    // consumer recovers seconds as `index * scale / rate = index / frame_rate`.
    let rate_f64 = (config.frame_rate * f64::from(IVF_TIMEBASE_SCALE)).round();
    if !rate_f64.is_finite() || rate_f64 < 1.0 || rate_f64 > f64::from(u32::MAX) {
        return Err(MuxError::SizeOverflow {
            context: "IVF frame-rate numerator",
        });
    }
    let rate_num = rate_f64 as u32;
    let frame_count = usize_to_u32(frames.len(), "IVF frame count")?;

    let mut header = [0u8; 32];
    header[0..4].copy_from_slice(b"DKIF");
    // version (0) and header length (32) are little-endian, like every IVF field.
    header[4..6].copy_from_slice(&0u16.to_le_bytes());
    header[6..8].copy_from_slice(&32u16.to_le_bytes());
    header[8..12].copy_from_slice(b"AV01");
    header[12..14].copy_from_slice(&(config.width as u16).to_le_bytes());
    header[14..16].copy_from_slice(&(config.height as u16).to_le_bytes());
    header[16..20].copy_from_slice(&rate_num.to_le_bytes());
    header[20..24].copy_from_slice(&IVF_TIMEBASE_SCALE.to_le_bytes());
    header[24..28].copy_from_slice(&frame_count.to_le_bytes());
    // bytes 28..32 are unused (left zero).
    writer.write_all(&header)?;

    for (index, frame) in frames.iter().enumerate() {
        let size = usize_to_u32(frame.av1_data.len(), "IVF frame size")?;
        let timestamp = index as u64;
        let mut frame_header = [0u8; 12];
        frame_header[0..4].copy_from_slice(&size.to_le_bytes());
        frame_header[4..12].copy_from_slice(&timestamp.to_le_bytes());
        writer.write_all(&frame_header)?;
        writer.write_all(&frame.av1_data)?;
    }

    Ok(())
}

/// Writes the bare AV1 low-overhead OBU elementary stream (`.obu`).
///
/// This is not a container: it is the AV1 temporal units concatenated in
/// presentation order with no framing of our own. rav1e already emits each
/// packet as a self-delimiting low-overhead OBU sequence (every OBU carries
/// its own size field), so concatenation yields a valid elementary stream that
/// `aomdec`, `dav1d`, and OBU parsers can walk directly. Intended for bitstream
/// inspection and codec-tooling interop, not playback.
///
/// # Errors
/// [`MuxError::Io`] if the output writer fails, or [`MuxError::NoFrames`] if
/// `frames` is empty.
pub fn write_obu_stream<W: Write>(writer: &mut W, frames: &[MuxFrame]) -> Result<(), MuxError> {
    if frames.is_empty() {
        return Err(MuxError::NoFrames);
    }
    for frame in frames {
        writer.write_all(&frame.av1_data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> Vec<MuxFrame> {
        vec![
            MuxFrame {
                av1_data: vec![0x0A, 0x0B, 0x0C, 0x0D],
                is_keyframe: true,
            },
            MuxFrame {
                av1_data: vec![0x01, 0x02],
                is_keyframe: false,
            },
        ]
    }

    fn cfg() -> Mp4Config {
        Mp4Config::new(30.0, 16, 16)
    }

    #[test]
    fn ivf_header_is_well_formed() {
        let mut out = Vec::new();
        write_ivf(&mut out, &cfg(), &frames()).expect("ivf write must succeed");

        assert_eq!(&out[0..4], b"DKIF", "missing IVF signature");
        assert_eq!(&out[8..12], b"AV01", "wrong codec fourcc");
        assert_eq!(u16::from_le_bytes([out[6], out[7]]), 32, "header length");
        assert_eq!(u16::from_le_bytes([out[12], out[13]]), 16, "width");
        assert_eq!(u16::from_le_bytes([out[14], out[15]]), 16, "height");
        // 30 fps → rate = 30 * 1000, scale = 1000.
        assert_eq!(
            u32::from_le_bytes([out[16], out[17], out[18], out[19]]),
            30_000
        );
        assert_eq!(
            u32::from_le_bytes([out[20], out[21], out[22], out[23]]),
            IVF_TIMEBASE_SCALE
        );
        assert_eq!(
            u32::from_le_bytes([out[24], out[25], out[26], out[27]]),
            2,
            "frame count"
        );
    }

    #[test]
    fn ivf_frame_records_carry_size_timestamp_and_payload() {
        let mut out = Vec::new();
        let f = frames();
        write_ivf(&mut out, &cfg(), &f).unwrap();

        // First frame record begins immediately after the 32-byte header.
        let r0_size = u32::from_le_bytes([out[32], out[33], out[34], out[35]]);
        assert_eq!(r0_size, 4);
        let r0_ts = u64::from_le_bytes([
            out[36], out[37], out[38], out[39], out[40], out[41], out[42], out[43],
        ]);
        assert_eq!(r0_ts, 0, "first frame timestamp must be 0");
        assert_eq!(&out[44..48], f[0].av1_data.as_slice());

        // Second frame record: 32 + 12 + 4 = 48 bytes in.
        let r1_size = u32::from_le_bytes([out[48], out[49], out[50], out[51]]);
        assert_eq!(r1_size, 2);
        let r1_ts = u64::from_le_bytes([
            out[52], out[53], out[54], out[55], out[56], out[57], out[58], out[59],
        ]);
        assert_eq!(r1_ts, 1, "second frame timestamp must be 1");
        assert_eq!(&out[60..62], f[1].av1_data.as_slice());
        assert_eq!(out.len(), 62, "exact IVF length for the fixture");
    }

    #[test]
    fn ivf_rejects_empty_and_bad_config() {
        assert!(matches!(
            write_ivf(&mut Vec::new(), &cfg(), &[]),
            Err(MuxError::NoFrames)
        ));
        let bad_rate = Mp4Config::new(0.0, 16, 16);
        assert!(write_ivf(&mut Vec::new(), &bad_rate, &frames()).is_err());
        let bad_dim = Mp4Config::new(30.0, 100_000, 16);
        assert!(write_ivf(&mut Vec::new(), &bad_dim, &frames()).is_err());
    }

    #[test]
    fn obu_stream_concatenates_temporal_units_in_order() {
        let mut out = Vec::new();
        let f = frames();
        write_obu_stream(&mut out, &f).expect("obu write must succeed");
        assert_eq!(out, vec![0x0A, 0x0B, 0x0C, 0x0D, 0x01, 0x02]);
    }

    #[test]
    fn obu_stream_rejects_empty() {
        assert!(matches!(
            write_obu_stream(&mut Vec::new(), &[]),
            Err(MuxError::NoFrames)
        ));
    }
}
