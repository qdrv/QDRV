// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! # qdrv-mux
//!
//! ISOBMFF (MP4) container muxer for QDRV delivery-tier AV1 streams.
//!
//! Writes a minimal but valid ISOBMFF file containing:
//! - `ftyp` box (file type: `isom` + `av01`)
//! - `moov` box with a single video track (`trak`) containing AV1 codec
//!   configuration and HDR colour signalling
//! - `mdat` box with the raw AV1 frame data
//!
//! This produces a `.mp4` file playable by any AV1-capable ISOBMFF player.
//! The implementation writes all ISOBMFF boxes directly as big-endian
//! byte sequences (as required by ISO 14496-12) — no external MP4 library
//! is required.
//!
//! ## Licence
//!
//! This crate is released under the GNU General Public Licence v2.0 (GPLv2).

use std::io::Write;

/// MPEG-standard video track timescale that preserves exact integer deltas for
/// common integer and NTSC-family frame rates.
pub const DEFAULT_MP4_TIMESCALE: u32 = 90_000;
/// Maximum stsz sample entries that still fit the 32-bit box size field.
const MAX_STSZ_SAMPLE_ENTRIES: usize = ((u32::MAX as usize) - 20) / 4;
/// Maximum `mdat` payload length that still fits classic 32-bit box sizing.
const MAX_MDAT_U32_PAYLOAD: u64 = u32::MAX as u64 - 8;
/// Serialized `ftyp` size emitted by [`write_ftyp`].
const FTYP_SIZE: u32 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOffsetBox {
    Stco,
    Co64,
}

fn usize_to_u32(value: usize, context: &'static str) -> Result<u32, MuxError> {
    u32::try_from(value).map_err(|_| MuxError::SizeOverflow { context })
}

/// Computes `8 + payload_size` as a checked `u32` box size. Centralises the
/// "ISOBMFF box header is 8 bytes, payload follows" arithmetic so every
/// inner box gets the same overflow handling.
fn box_size_u32(payload_size: usize, context: &'static str) -> Result<u32, MuxError> {
    let total = payload_size
        .checked_add(8)
        .ok_or(MuxError::SizeOverflow { context })?;
    usize_to_u32(total, context)
}

fn ensure_sample_table_bounds(sample_count: usize) -> Result<(), MuxError> {
    if sample_count > MAX_STSZ_SAMPLE_ENTRIES {
        return Err(MuxError::SizeOverflow {
            context: "stsz sample table entry count",
        });
    }
    Ok(())
}

fn mdat_header_size(payload_len: u64) -> u32 {
    if payload_len > MAX_MDAT_U32_PAYLOAD {
        16
    } else {
        8
    }
}

fn write_mdat_header<W: Write>(writer: &mut W, payload_len: u64) -> Result<(), MuxError> {
    if payload_len > MAX_MDAT_U32_PAYLOAD {
        let mdat_size = 16u64
            .checked_add(payload_len)
            .ok_or(MuxError::SizeOverflow {
                context: "mdat largesize box size",
            })?;
        writer.write_all(&1u32.to_be_bytes())?;
        writer.write_all(b"mdat")?;
        writer.write_all(&mdat_size.to_be_bytes())?;
    } else {
        let payload_u32 = u32::try_from(payload_len).map_err(|_| MuxError::SizeOverflow {
            context: "mdat payload length",
        })?;
        let mdat_size = 8u32
            .checked_add(payload_u32)
            .ok_or(MuxError::SizeOverflow {
                context: "mdat box size",
            })?;
        writer.write_all(&mdat_size.to_be_bytes())?;
        writer.write_all(b"mdat")?;
    }
    Ok(())
}

fn compute_data_offset(moov_len: u32, mdat_header_size: u32) -> Result<u64, MuxError> {
    u64::from(FTYP_SIZE)
        .checked_add(u64::from(moov_len))
        .and_then(|v| v.checked_add(u64::from(mdat_header_size)))
        .ok_or(MuxError::SizeOverflow {
            context: "chunk offset computation",
        })
}

fn chunk_offset_box_for_data_offset(data_offset: u64) -> ChunkOffsetBox {
    if data_offset > u64::from(u32::MAX) {
        ChunkOffsetBox::Co64
    } else {
        ChunkOffsetBox::Stco
    }
}

/// Configuration for the MP4 muxer.
#[derive(Debug, Clone)]
pub struct Mp4Config {
    /// Frame rate (frames per second) for timing calculations.
    pub frame_rate: f64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

impl Mp4Config {
    /// Constructs a muxer config from explicit dimensions and frame rate.
    ///
    /// Prefer this over `Mp4Config::default()` when the caller has the
    /// actual frame geometry — accidentally muxing AV1 frames with
    /// container dimensions that don't match the bitstream is undefined
    /// in ISOBMFF and silently produces a non-conforming file (audit
    /// finding AA-2).
    pub fn new(frame_rate: f64, width: u32, height: u32) -> Self {
        Self {
            frame_rate,
            width,
            height,
        }
    }
}

impl Default for Mp4Config {
    /// **Test-only defaults.** `frame_rate = 24.0`, `width = 256`,
    /// `height = 64` — matching the QDRV `write-test` fixture geometry.
    /// Production callers should construct via [`Mp4Config::new`] (or
    /// initialise the fields explicitly) with the *actual* AV1 frame
    /// dimensions; the muxer does not cross-check `config.width`/`height`
    /// against the AV1 bitstream's declared geometry, so a default-config
    /// mux of a real 1920×1080 stream will write an MP4 declaring
    /// 256×64 (audit finding AA-2).
    fn default() -> Self {
        Self {
            frame_rate: 24.0,
            width: 256,
            height: 64,
        }
    }
}

/// An encoded frame ready for muxing.
pub struct MuxFrame {
    /// AV1 bitstream bytes for this frame.
    pub av1_data: Vec<u8>,
    /// `true` if this frame is a keyframe.
    pub is_keyframe: bool,
}

/// Writes a complete ISOBMFF (MP4) file containing the given AV1 frames.
///
/// Produces a valid ISO Base Media File Format container with a single AV1
/// video track. The file can be played by any standards-compliant AV1 player.
///
/// # Arguments
/// * `writer` — Output stream.
/// * `config` — Muxer configuration (dimensions, frame rate).
/// * `frames` — Encoded AV1 frames in presentation order.
///
/// # Example
///
/// ```
/// use qdrv_mux::{Mp4Config, MuxFrame, write_mp4};
///
/// // Construct dimensions explicitly via `Mp4Config::new` — the
/// // `Default` impl carries test-only sizes (256×64); production
/// // callers should pass the real AV1 frame dimensions.
/// let config = Mp4Config::new(24.0, 16, 16);
/// // The AV1 bitstream bytes here are placeholder dummy data; in real
/// // use these are emitted by `qdrv_codec::TemporalEncoder`.
/// let frames = vec![MuxFrame {
///     av1_data: vec![0x12, 0x00, 0x0A, 0x0A],
///     is_keyframe: true,
/// }];
/// let mut out: Vec<u8> = Vec::new();
/// write_mp4(&mut out, &config, &frames).expect("mux must succeed");
/// // The ISOBMFF `ftyp` box is the very first box in every MP4 file.
/// assert_eq!(&out[4..8], b"ftyp");
/// ```
pub fn write_mp4<W: Write>(
    writer: &mut W,
    config: &Mp4Config,
    frames: &[MuxFrame],
) -> Result<(), MuxError> {
    if frames.is_empty() {
        return Err(MuxError::NoFrames);
    }
    if config.frame_rate <= 0.0 || !config.frame_rate.is_finite() {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "frame_rate must be positive and finite, got {}",
                config.frame_rate
            ),
        )));
    }
    if config.width > u16::MAX as u32 || config.height > u16::MAX as u32 {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "MP4 sample entry dimensions exceed u16 range: {}x{} (max {}x{})",
                config.width,
                config.height,
                u16::MAX,
                u16::MAX
            ),
        )));
    }

    let timescale: u32 = DEFAULT_MP4_TIMESCALE;
    let frame_duration_f64 = (timescale as f64 / config.frame_rate).round();
    if !frame_duration_f64.is_finite()
        || frame_duration_f64 < 1.0
        || frame_duration_f64 > u32::MAX as f64
    {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "computed frame_duration out of u32 range (timescale={timescale}, frame_rate={}, value={frame_duration_f64})",
                config.frame_rate
            ),
        )));
    }
    let frame_duration = frame_duration_f64 as u32;
    if frame_duration == 0 {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "computed frame_duration is 0 (timescale={timescale}, frame_rate={}); \
                 increase frame_rate or timescale",
                config.frame_rate
            ),
        )));
    }
    let frame_count_u64 = u64::try_from(frames.len()).map_err(|_| MuxError::SizeOverflow {
        context: "frame count conversion to u64",
    })?;
    ensure_sample_table_bounds(frames.len())?;
    let total_duration = u64::from(frame_duration)
        .checked_mul(frame_count_u64)
        .ok_or(MuxError::SizeOverflow {
            context: "total movie duration",
        })?;
    let sample_count = usize_to_u32(frames.len(), "sample count")?;

    let mut sample_sizes: Vec<u32> = Vec::with_capacity(frames.len());
    let mut sync_samples: Vec<u32> = Vec::new();
    let mut mdat_payload = Vec::new();

    for (i, f) in frames.iter().enumerate() {
        sample_sizes.push(usize_to_u32(f.av1_data.len(), "sample size")?);
        if f.is_keyframe {
            sync_samples.push(usize_to_u32(i + 1, "sync sample index")?);
        }
        mdat_payload
            .try_reserve(f.av1_data.len())
            .map_err(|_| MuxError::SizeOverflow {
                context: "mdat payload allocation",
            })?;
        mdat_payload.extend_from_slice(&f.av1_data);
    }

    let mut ctx = MoovBuildCtx {
        config,
        timescale,
        frame_duration,
        total_duration,
        sample_count,
        chunk_offset_box: ChunkOffsetBox::Stco,
    };
    let mut moov = build_moov(&ctx, &sample_sizes, &sync_samples)?;

    let mdat_payload_len =
        u64::try_from(mdat_payload.len()).map_err(|_| MuxError::SizeOverflow {
            context: "mdat payload length conversion",
        })?;
    let mdat_header_size = mdat_header_size(mdat_payload_len);

    // Patch the chunk offset (`stco` for 32-bit, `co64` for 64-bit) to point
    // to the first `mdat` sample byte. Layout: ftyp + moov + mdat header.
    let mut moov_len_u32 = usize_to_u32(moov.len(), "moov box length")?;
    let mut data_offset_u64 = compute_data_offset(moov_len_u32, mdat_header_size)?;
    if chunk_offset_box_for_data_offset(data_offset_u64) == ChunkOffsetBox::Co64 {
        ctx.chunk_offset_box = ChunkOffsetBox::Co64;
        moov = build_moov(&ctx, &sample_sizes, &sync_samples)?;
        moov_len_u32 = usize_to_u32(moov.len(), "moov box length")?;
        data_offset_u64 = compute_data_offset(moov_len_u32, mdat_header_size)?;
    }
    patch_chunk_offset(&mut moov, data_offset_u64)?;

    write_ftyp(writer)?;
    writer.write_all(&moov)?;
    write_mdat_header(writer, mdat_payload_len)?;
    writer.write_all(&mdat_payload)?;

    Ok(())
}

/// Errors from the MP4 muxer.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// Returned when `write_mp4` is invoked with an empty frame list, because the
    /// `moov`/`mdat` layout requires at least one sample to describe timing and sizes.
    #[error("no frames provided")]
    NoFrames,
    /// Wraps an underlying `std::io::Error` from the output writer (disk full, broken
    /// pipe, permission denied, and similar transport failures).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Integer conversion or arithmetic overflow while building box sizes and offsets.
    #[error("MP4 size overflow while computing {context}")]
    SizeOverflow { context: &'static str },
}

fn write_ftyp<W: Write>(w: &mut W) -> Result<(), MuxError> {
    let mut buf = Vec::with_capacity(24);
    let size: u32 = 24;
    buf.extend_from_slice(&size.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(b"isom"); // major brand
    buf.extend_from_slice(&0u32.to_be_bytes()); // minor version
    buf.extend_from_slice(b"isom"); // compatible brand
    buf.extend_from_slice(b"av01"); // compatible brand
    w.write_all(&buf)?;
    Ok(())
}

/// Shared parameters threaded through the `moov` → `trak` → `mdia` →
/// `minf` → `stbl` builder chain.
///
/// Every builder in the chain needs the same timing + sample-table +
/// chunk-offset state. Passing one borrowed context instead of 6–8
/// individual positional arguments at each level keeps the call sites
/// short, removes drift hazards (mismatched orderings between calls),
/// and lets future additions land in one place. Per-pixel / per-frame
/// state (sample sizes and sync sample table) stays out of the context
/// so the builders that don't need them aren't forced to acknowledge
/// them.
struct MoovBuildCtx<'a> {
    config: &'a Mp4Config,
    timescale: u32,
    frame_duration: u32,
    total_duration: u64,
    sample_count: u32,
    chunk_offset_box: ChunkOffsetBox,
}

fn build_moov(
    ctx: &MoovBuildCtx<'_>,
    sample_sizes: &[u32],
    sync_samples: &[u32],
) -> Result<Vec<u8>, MuxError> {
    // ISOBMFF hierarchy built here: `moov` (movie) contains `mvhd` (movie header) plus
    // one `trak` (track). That `trak` nests `tkhd` and `mdia`; `mdia` chains `mdhd`,
    // `hdlr`, and `minf` → `vmhd`/`dinf`/`stbl` with sample tables (`stsd`, `stts`, …).
    let trak = build_trak(ctx, sample_sizes, sync_samples)?;

    let mvhd = build_mvhd(ctx.timescale, ctx.total_duration);
    let payload = mvhd
        .len()
        .checked_add(trak.len())
        .ok_or(MuxError::SizeOverflow {
            context: "moov payload size",
        })?;
    let moov_size = box_size_u32(payload, "moov box size")?;
    let mut moov = Vec::with_capacity(moov_size as usize);
    moov.extend_from_slice(&moov_size.to_be_bytes());
    moov.extend_from_slice(b"moov");
    moov.extend_from_slice(&mvhd);
    moov.extend_from_slice(&trak);
    Ok(moov)
}

fn build_mvhd(timescale: u32, duration: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(120);
    let size: u32 = 120;
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"mvhd");
    b.extend_from_slice(&0x01000000u32.to_be_bytes()); // FullBox: version=1, flags=0
    b.extend_from_slice(&0u64.to_be_bytes()); // creation time
    b.extend_from_slice(&0u64.to_be_bytes()); // modification time
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&duration.to_be_bytes());
    b.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate = 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume = 1.0
    b.extend_from_slice(&[0u8; 10]); // reserved
    // identity matrix (9 × u32)
    for &v in &[0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0u8; 24]); // pre-defined
    b.extend_from_slice(&2u32.to_be_bytes()); // next track ID
    b
}

fn build_trak(
    ctx: &MoovBuildCtx<'_>,
    sample_sizes: &[u32],
    sync_samples: &[u32],
) -> Result<Vec<u8>, MuxError> {
    let tkhd = build_tkhd(ctx.config, ctx.total_duration);
    let mdia = build_mdia(ctx, sample_sizes, sync_samples)?;

    let payload = tkhd
        .len()
        .checked_add(mdia.len())
        .ok_or(MuxError::SizeOverflow {
            context: "trak payload size",
        })?;
    let trak_size = box_size_u32(payload, "trak box size")?;
    let mut trak = Vec::with_capacity(trak_size as usize);
    trak.extend_from_slice(&trak_size.to_be_bytes());
    trak.extend_from_slice(b"trak");
    trak.extend_from_slice(&tkhd);
    trak.extend_from_slice(&mdia);
    Ok(trak)
}

fn build_tkhd(config: &Mp4Config, duration: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(104);
    let size: u32 = 104;
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"tkhd");
    b.extend_from_slice(&0x01000003u32.to_be_bytes()); // version 1 + flags (enabled, in-movie, in-preview)
    b.extend_from_slice(&0u64.to_be_bytes()); // creation time
    b.extend_from_slice(&0u64.to_be_bytes()); // modification time
    b.extend_from_slice(&1u32.to_be_bytes()); // track ID
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&duration.to_be_bytes());
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&0u16.to_be_bytes()); // layer
    b.extend_from_slice(&0u16.to_be_bytes()); // alternate group
    b.extend_from_slice(&0u16.to_be_bytes()); // volume (0 for video)
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    for &v in &[0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&(config.width << 16).to_be_bytes());
    b.extend_from_slice(&(config.height << 16).to_be_bytes());
    b
}

fn build_mdia(
    ctx: &MoovBuildCtx<'_>,
    sample_sizes: &[u32],
    sync_samples: &[u32],
) -> Result<Vec<u8>, MuxError> {
    let mdhd = build_mdhd(ctx.timescale, ctx.total_duration);
    let hdlr = build_hdlr();
    let minf = build_minf(ctx, sample_sizes, sync_samples)?;

    let payload = mdhd
        .len()
        .checked_add(hdlr.len())
        .and_then(|n| n.checked_add(minf.len()))
        .ok_or(MuxError::SizeOverflow {
            context: "mdia payload size",
        })?;
    let mdia_size = box_size_u32(payload, "mdia box size")?;
    let mut mdia = Vec::with_capacity(mdia_size as usize);
    mdia.extend_from_slice(&mdia_size.to_be_bytes());
    mdia.extend_from_slice(b"mdia");
    mdia.extend_from_slice(&mdhd);
    mdia.extend_from_slice(&hdlr);
    mdia.extend_from_slice(&minf);
    Ok(mdia)
}

fn build_mdhd(timescale: u32, duration: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(44);
    let size: u32 = 44;
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"mdhd");
    b.extend_from_slice(&0x01000000u32.to_be_bytes()); // FullBox: version=1, flags=0
    b.extend_from_slice(&0u64.to_be_bytes()); // creation time
    b.extend_from_slice(&0u64.to_be_bytes()); // modification time
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&duration.to_be_bytes());
    b.extend_from_slice(&0x55C40000u32.to_be_bytes()); // language = 'und' + pad
    b
}

fn build_hdlr() -> Vec<u8> {
    let name = b"QDRV Video\0";
    // hdlr box: size(4) + type(4) + version+flags(4) + pre_defined(4) +
    //           handler_type(4) + reserved(12) + name(variable)
    let size = (8 + 4 + 4 + 4 + 12 + name.len()) as u32;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"hdlr");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&0u32.to_be_bytes()); // pre-defined
    b.extend_from_slice(b"vide"); // handler type
    b.extend_from_slice(&[0u8; 12]); // reserved
    b.extend_from_slice(name);
    b
}

fn build_minf(
    ctx: &MoovBuildCtx<'_>,
    sample_sizes: &[u32],
    sync_samples: &[u32],
) -> Result<Vec<u8>, MuxError> {
    let vmhd = build_vmhd();
    let dinf = build_dinf();
    let stbl = build_stbl(ctx, sample_sizes, sync_samples)?;

    let payload = vmhd
        .len()
        .checked_add(dinf.len())
        .and_then(|n| n.checked_add(stbl.len()))
        .ok_or(MuxError::SizeOverflow {
            context: "minf payload size",
        })?;
    let minf_size = box_size_u32(payload, "minf box size")?;
    let mut minf = Vec::with_capacity(minf_size as usize);
    minf.extend_from_slice(&minf_size.to_be_bytes());
    minf.extend_from_slice(b"minf");
    minf.extend_from_slice(&vmhd);
    minf.extend_from_slice(&dinf);
    minf.extend_from_slice(&stbl);
    Ok(minf)
}

fn build_vmhd() -> Vec<u8> {
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&20u32.to_be_bytes());
    b.extend_from_slice(b"vmhd");
    b.extend_from_slice(&1u32.to_be_bytes()); // version 0 + flags=1
    b.extend_from_slice(&0u16.to_be_bytes()); // graphics mode
    b.extend_from_slice(&[0u8; 6]); // op colour
    b
}

fn build_dinf() -> Vec<u8> {
    // dinf > dref with one url entry (self-contained)
    let url_size: u32 = 12;
    let dref_size: u32 = 16 + url_size;
    let dinf_size: u32 = 8 + dref_size;

    let mut b = Vec::with_capacity(dinf_size as usize);
    b.extend_from_slice(&dinf_size.to_be_bytes());
    b.extend_from_slice(b"dinf");
    b.extend_from_slice(&dref_size.to_be_bytes());
    b.extend_from_slice(b"dref");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&1u32.to_be_bytes()); // entry count
    b.extend_from_slice(&url_size.to_be_bytes());
    b.extend_from_slice(b"url ");
    b.extend_from_slice(&1u32.to_be_bytes()); // flags = self-contained
    b
}

fn build_stbl(
    ctx: &MoovBuildCtx<'_>,
    sample_sizes: &[u32],
    sync_samples: &[u32],
) -> Result<Vec<u8>, MuxError> {
    let stsd = build_stsd(ctx.config)?;
    let stts_runs = [(ctx.sample_count, ctx.frame_duration)];
    let stts = build_stts(&stts_runs)?;
    let stsz = build_stsz(sample_sizes)?;
    let stsc = build_stsc(ctx.sample_count);
    let chunk_offsets = build_chunk_offset_box(ctx.chunk_offset_box);
    let stss = if sync_samples.len() < ctx.sample_count as usize {
        build_stss(sync_samples)?
    } else {
        Vec::new()
    };

    let payload = stsd
        .len()
        .checked_add(stts.len())
        .and_then(|n| n.checked_add(stsz.len()))
        .and_then(|n| n.checked_add(stsc.len()))
        .and_then(|n| n.checked_add(chunk_offsets.len()))
        .and_then(|n| n.checked_add(stss.len()))
        .ok_or(MuxError::SizeOverflow {
            context: "stbl payload size",
        })?;
    let stbl_size = box_size_u32(payload, "stbl box size")?;
    let mut stbl = Vec::with_capacity(stbl_size as usize);
    stbl.extend_from_slice(&stbl_size.to_be_bytes());
    stbl.extend_from_slice(b"stbl");
    stbl.extend_from_slice(&stsd);
    stbl.extend_from_slice(&stts);
    stbl.extend_from_slice(&stsz);
    stbl.extend_from_slice(&stsc);
    stbl.extend_from_slice(&chunk_offsets);
    stbl.extend_from_slice(&stss);
    Ok(stbl)
}

fn build_stsd(config: &Mp4Config) -> Result<Vec<u8>, MuxError> {
    let av1c = build_av1c();
    let colr = build_colr_nclx();
    // av01 sample entry: 8-byte box header + 78-byte VisualSampleEntry +
    // av1C child + colr child. The colr nclx box signals BT.2020 primaries,
    // SMPTE ST 2084 transfer, and BT.2020 NCL matrix coefficients with full
    // pixel range — matching the QDRV delivery tier and making the HDR
    // characteristics visible to MP4-level players that don't parse AV1 OBUs.
    let av01_payload = 78usize
        .checked_add(av1c.len())
        .and_then(|n| n.checked_add(colr.len()))
        .ok_or(MuxError::SizeOverflow {
            context: "av01 sample entry payload",
        })?;
    let av01_size = box_size_u32(av01_payload, "av01 sample entry size")?;

    let mut av01 = Vec::with_capacity(av01_size as usize);
    av01.extend_from_slice(&av01_size.to_be_bytes());
    av01.extend_from_slice(b"av01");
    av01.extend_from_slice(&[0u8; 6]); // reserved
    av01.extend_from_slice(&1u16.to_be_bytes()); // data ref index
    av01.extend_from_slice(&[0u8; 16]); // pre-defined + reserved
    av01.extend_from_slice(&(config.width as u16).to_be_bytes());
    av01.extend_from_slice(&(config.height as u16).to_be_bytes());
    av01.extend_from_slice(&0x00480000u32.to_be_bytes()); // h resolution 72 dpi
    av01.extend_from_slice(&0x00480000u32.to_be_bytes()); // v resolution 72 dpi
    av01.extend_from_slice(&0u32.to_be_bytes()); // reserved
    av01.extend_from_slice(&1u16.to_be_bytes()); // frame count
    av01.extend_from_slice(&[0u8; 32]); // compressor name
    av01.extend_from_slice(&0x0018u16.to_be_bytes()); // depth = 24
    av01.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre-defined = -1
    av01.extend_from_slice(&av1c);
    av01.extend_from_slice(&colr);

    // stsd: version+flags (4) + entry_count (4) + entries
    let stsd_payload = av01.len().checked_add(8).ok_or(MuxError::SizeOverflow {
        context: "stsd payload size",
    })?;
    let stsd_size = box_size_u32(stsd_payload, "stsd box size")?;
    let mut stsd = Vec::with_capacity(stsd_size as usize);
    stsd.extend_from_slice(&stsd_size.to_be_bytes());
    stsd.extend_from_slice(b"stsd");
    stsd.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    stsd.extend_from_slice(&1u32.to_be_bytes()); // entry count
    stsd.extend_from_slice(&av01);
    Ok(stsd)
}

/// Builds a `colr` Colour Information Box with `nclx` colour type for
/// QDRV delivery-tier HDR signalling.
///
/// Constants:
/// - `colour_primaries = 9` (BT.2020)
/// - `transfer_characteristics = 16` (SMPTE ST 2084)
/// - `matrix_coefficients = 9` (BT.2020 non-constant luminance)
/// - `full_range_flag = 1` (full pixel range, matches the encoder)
fn build_colr_nclx() -> Vec<u8> {
    // size(4) + 'colr'(4) + 'nclx'(4) + primaries(2) + transfer(2)
    //   + matrix(2) + full_range_flag(1) = 19 bytes total
    let mut b = Vec::with_capacity(19);
    let size: u32 = 19;
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"colr");
    b.extend_from_slice(b"nclx");
    b.extend_from_slice(&9u16.to_be_bytes()); // BT.2020 primaries
    b.extend_from_slice(&16u16.to_be_bytes()); // SMPTE ST 2084 transfer
    b.extend_from_slice(&9u16.to_be_bytes()); // BT.2020 NCL matrix
    b.push(0x80); // full_range_flag = 1, reserved = 0
    b
}

fn build_av1c() -> Vec<u8> {
    // Minimal AV1 Codec Configuration Record per
    // <https://aomediacodec.github.io/av1-isobmff/#av1codecconfigurationbox-syntax>.
    // Four payload bytes after the `av1C` four-cc:
    //
    // - Byte 0 = `0x81`:
    //     marker (1 bit, must be 1)        = 1
    //     version (7 bits, currently 1)    = 0000001     → 0b1000_0001 = 0x81
    // - Byte 1 = `0x44`:
    //     seq_profile (3 bits)             = 010 (Profile 2: 12-bit 4:4:4)
    //     seq_level_idx_0 (5 bits)         = 00100 (level 4.0)
    //                                                  → 0b0100_0100 = 0x44
    // - Byte 2 = `0x60`:
    //     seq_tier_0 (1 bit)               = 0
    //     high_bitdepth (1 bit)            = 1
    //     twelve_bit (1 bit)               = 1
    //     monochrome (1 bit)               = 0
    //     chroma_subsampling_x (1 bit)     = 0
    //     chroma_subsampling_y (1 bit)     = 0
    //     chroma_sample_position (2 bits)  = 00
    //                                                  → 0b0110_0000 = 0x60
    // - Byte 3 = `0x00`:
    //     reserved (3 bits)                          = 000
    //     initial_presentation_delay_present (1 bit) = 0
    //     initial_presentation_delay_minus_one OR
    //       reserved (4 bits)                        = 0000
    //                                                  → 0b0000_0000 = 0x00
    //
    // AA-4: the previous comment was imprecise about byte 3; the correct
    // breakdown is captured above. The byte values themselves are correct
    // and match what mainstream AV1-in-MP4 demuxers expect.
    let mut b = Vec::with_capacity(12);
    let size: u32 = 12;
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"av1C");
    b.push(0x81);
    b.push(0x44);
    b.push(0x60);
    b.push(0x00);
    b
}

fn build_stts(runs: &[(u32, u32)]) -> Result<Vec<u8>, MuxError> {
    let entry_count = usize_to_u32(runs.len(), "stts entry count")?;
    // payload = version+flags (4) + entry_count (4) + 8 bytes per entry
    let entry_bytes = (runs.len()).checked_mul(8).ok_or(MuxError::SizeOverflow {
        context: "stts entry bytes",
    })?;
    let payload = entry_bytes.checked_add(8).ok_or(MuxError::SizeOverflow {
        context: "stts payload size",
    })?;
    let size = box_size_u32(payload, "stts box size")?;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"stts");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&entry_count.to_be_bytes());
    for &(sample_count, sample_delta) in runs {
        b.extend_from_slice(&sample_count.to_be_bytes());
        b.extend_from_slice(&sample_delta.to_be_bytes());
    }
    Ok(b)
}

fn build_stsz(sample_sizes: &[u32]) -> Result<Vec<u8>, MuxError> {
    let entry_count = usize_to_u32(sample_sizes.len(), "stsz entry count")?;
    // payload = version+flags (4) + sample_size (4) + entry_count (4) + 4 per entry
    let entry_bytes = sample_sizes
        .len()
        .checked_mul(4)
        .ok_or(MuxError::SizeOverflow {
            context: "stsz entry bytes",
        })?;
    let payload = entry_bytes.checked_add(12).ok_or(MuxError::SizeOverflow {
        context: "stsz payload size",
    })?;
    let size = box_size_u32(payload, "stsz box size")?;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"stsz");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 (variable)
    b.extend_from_slice(&entry_count.to_be_bytes());
    for &s in sample_sizes {
        b.extend_from_slice(&s.to_be_bytes());
    }
    Ok(b)
}

fn build_stsc(sample_count: u32) -> Vec<u8> {
    let size: u32 = 28;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"stsc");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&1u32.to_be_bytes()); // entry count
    b.extend_from_slice(&1u32.to_be_bytes()); // first chunk
    b.extend_from_slice(&sample_count.to_be_bytes()); // samples per chunk
    b.extend_from_slice(&1u32.to_be_bytes()); // sample description index
    b
}

fn build_stco() -> Vec<u8> {
    let size: u32 = 20;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"stco");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&1u32.to_be_bytes()); // entry count
    b.extend_from_slice(&0u32.to_be_bytes()); // chunk offset (patched by patch_chunk_offset)
    b
}

fn build_co64() -> Vec<u8> {
    let size: u32 = 24;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"co64");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&1u32.to_be_bytes()); // entry count
    b.extend_from_slice(&0u64.to_be_bytes()); // chunk offset (patched by patch_chunk_offset)
    b
}

fn build_chunk_offset_box(kind: ChunkOffsetBox) -> Vec<u8> {
    match kind {
        ChunkOffsetBox::Stco => build_stco(),
        ChunkOffsetBox::Co64 => build_co64(),
    }
}

fn find_child_box(data: &[u8], start: usize, end: usize, typ: &[u8; 4]) -> Option<(usize, usize)> {
    let mut cursor = start;
    while cursor.checked_add(8)? <= end {
        let size = u32::from_be_bytes(data[cursor..cursor + 4].try_into().ok()?) as usize;
        if size < 8 {
            return None;
        }
        let box_end = cursor.checked_add(size)?;
        if box_end > end {
            return None;
        }
        if &data[cursor + 4..cursor + 8] == typ {
            return Some((cursor, box_end));
        }
        cursor = box_end;
    }
    None
}

fn locate_chunk_offset_entry(moov: &[u8]) -> Option<(ChunkOffsetBox, usize)> {
    // moov box header
    if moov.len() < 8 || &moov[4..8] != b"moov" {
        return None;
    }
    let moov_size = u32::from_be_bytes(moov[0..4].try_into().ok()?) as usize;
    if moov_size < 8 || moov_size > moov.len() {
        return None;
    }

    // moov -> trak -> mdia -> minf -> stbl -> {stco|co64}
    let (trak_start, trak_end) = find_child_box(moov, 8, moov_size, b"trak")?;
    let (mdia_start, mdia_end) = find_child_box(moov, trak_start + 8, trak_end, b"mdia")?;
    let (minf_start, minf_end) = find_child_box(moov, mdia_start + 8, mdia_end, b"minf")?;
    let (stbl_start, stbl_end) = find_child_box(moov, minf_start + 8, minf_end, b"stbl")?;
    if let Some((co64_start, co64_end)) = find_child_box(moov, stbl_start + 8, stbl_end, b"co64") {
        if co64_end.checked_sub(co64_start)? < 24 {
            return None;
        }
        return Some((ChunkOffsetBox::Co64, co64_start + 16));
    }
    let (stco_start, stco_end) = find_child_box(moov, stbl_start + 8, stbl_end, b"stco")?;
    if stco_end.checked_sub(stco_start)? < 20 {
        return None;
    }
    Some((ChunkOffsetBox::Stco, stco_start + 16))
}

/// Patches the chunk offset box (`stco` or `co64`) inside a serialised moov.
fn patch_chunk_offset(moov: &mut [u8], data_offset: u64) -> Result<(), MuxError> {
    let (kind, offset_pos) = locate_chunk_offset_entry(moov).ok_or_else(|| {
        MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "failed to locate stco/co64 chunk offset entry in moov box",
        ))
    })?;
    let (end, bytes): (usize, [u8; 8]) = match kind {
        ChunkOffsetBox::Stco => {
            let offset_u32 = u32::try_from(data_offset).map_err(|_| MuxError::SizeOverflow {
                context: "stco chunk offset requires co64",
            })?;
            let end = offset_pos.checked_add(4).ok_or(MuxError::SizeOverflow {
                context: "stco offset patch range",
            })?;
            let mut b = [0u8; 8];
            b[4..8].copy_from_slice(&offset_u32.to_be_bytes());
            (end, b)
        }
        ChunkOffsetBox::Co64 => {
            let end = offset_pos.checked_add(8).ok_or(MuxError::SizeOverflow {
                context: "co64 offset patch range",
            })?;
            (end, data_offset.to_be_bytes())
        }
    };
    if end > moov.len() {
        return Err(MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk offset entry range exceeds moov bounds",
        )));
    }
    match kind {
        ChunkOffsetBox::Stco => moov[offset_pos..end].copy_from_slice(&bytes[4..8]),
        ChunkOffsetBox::Co64 => moov[offset_pos..end].copy_from_slice(&bytes),
    }
    Ok(())
}

fn build_stss(sync_samples: &[u32]) -> Result<Vec<u8>, MuxError> {
    let entry_count = usize_to_u32(sync_samples.len(), "stss entry count")?;
    // payload = version+flags (4) + entry_count (4) + 4 per entry
    let entry_bytes = sync_samples
        .len()
        .checked_mul(4)
        .ok_or(MuxError::SizeOverflow {
            context: "stss entry bytes",
        })?;
    let payload = entry_bytes.checked_add(8).ok_or(MuxError::SizeOverflow {
        context: "stss payload size",
    })?;
    let size = box_size_u32(payload, "stss box size")?;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(b"stss");
    b.extend_from_slice(&0u32.to_be_bytes()); // version + flags
    b.extend_from_slice(&entry_count.to_be_bytes());
    for &s in sync_samples {
        b.extend_from_slice(&s.to_be_bytes());
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a one-frame MP4 and asserts the artefact contains `ftyp`, `moov`, and
    /// `mdat` boxes with plausible overall size (sanity check for the muxing pipeline).
    #[test]
    fn test_write_mp4_single_frame() {
        let config = Mp4Config {
            frame_rate: 24.0,
            width: 16,
            height: 16,
        };
        let frames = vec![MuxFrame {
            av1_data: vec![0x12, 0x00, 0x0A, 0x0A], // dummy AV1 bytes
            is_keyframe: true,
        }];

        let mut output = Vec::new();
        write_mp4(&mut output, &config, &frames).unwrap();

        assert!(
            output.len() > 24,
            "MP4 output too small: {} bytes",
            output.len()
        );
        assert_eq!(&output[4..8], b"ftyp");
        assert!(output.windows(4).any(|w| w == b"moov"));
        assert!(output.windows(4).any(|w| w == b"mdat"));
    }

    /// Exercises multi-frame timing and sync-sample signalling, verifying that the
    /// serialised bitstream still embeds the `av01` sample entry and a `stss` keyframe map.
    #[test]
    fn test_write_mp4_multiple_frames() {
        let config = Mp4Config {
            frame_rate: 30.0,
            width: 8,
            height: 8,
        };
        let frames: Vec<MuxFrame> = (0..5)
            .map(|i| MuxFrame {
                av1_data: vec![0xAA; 100],
                is_keyframe: i == 0,
            })
            .collect();

        let mut output = Vec::new();
        write_mp4(&mut output, &config, &frames).unwrap();

        assert!(output.len() > 500);
        assert!(output.windows(4).any(|w| w == b"av01"));
        assert!(output.windows(4).any(|w| w == b"stss"));
    }

    /// Confirms the muxer rejects an empty frame slice up front, avoiding invalid MP4s
    /// with undefined sample counts or zero-length `mdat` payloads.
    #[test]
    fn test_write_mp4_rejects_empty() {
        let config = Mp4Config::default();
        let mut output = Vec::new();
        assert!(write_mp4(&mut output, &config, &[]).is_err());
    }

    /// A pathological frame rate can round `timescale / frame_rate` down to zero; the muxer
    /// must reject that case instead of emitting an invalid `stts` entry with zero deltas.
    #[test]
    fn test_write_mp4_rejects_zero_frame_duration() {
        let config = Mp4Config {
            frame_rate: 1.0e9,
            width: 16,
            height: 16,
        };
        let frames = vec![MuxFrame {
            av1_data: vec![0x00],
            is_keyframe: true,
        }];
        let mut output = Vec::new();
        assert!(write_mp4(&mut output, &config, &frames).is_err());
    }

    #[test]
    fn test_write_mp4_rejects_dimensions_over_u16() {
        let config = Mp4Config {
            frame_rate: 24.0,
            width: u16::MAX as u32 + 1,
            height: 16,
        };
        let frames = vec![MuxFrame {
            av1_data: vec![0x00],
            is_keyframe: true,
        }];
        let mut output = Vec::new();
        assert!(write_mp4(&mut output, &config, &frames).is_err());
    }

    #[test]
    fn test_build_av1c_signals_profile2_for_12bit_444() {
        let av1c = build_av1c();
        assert_eq!(av1c[8], 0x81);
        assert_eq!(av1c[9], 0x44);
        assert_eq!(av1c[10], 0x60);
    }

    #[test]
    fn test_patch_chunk_offset_ignores_non_box_fourcc_bytes() {
        let config = Mp4Config {
            frame_rate: 24.0,
            width: 16,
            height: 16,
        };
        // Include a sample size whose big-endian bytes equal "stco" to ensure
        // patching follows box structure rather than raw byte scanning.
        let sample_sizes = [u32::from_be_bytes(*b"stco"), 1234];
        let sync_samples = [1u32];
        let ctx = MoovBuildCtx {
            config: &config,
            timescale: DEFAULT_MP4_TIMESCALE,
            frame_duration: 3750,
            total_duration: 7500,
            sample_count: 2,
            chunk_offset_box: ChunkOffsetBox::Stco,
        };
        let mut moov = build_moov(&ctx, &sample_sizes, &sync_samples).unwrap();

        let (kind, offset_pos) = locate_chunk_offset_entry(&moov).expect("stco box should exist");
        assert_eq!(kind, ChunkOffsetBox::Stco);
        assert_eq!(&moov[offset_pos..offset_pos + 4], &[0, 0, 0, 0]);

        patch_chunk_offset(&mut moov, 0x11223344).unwrap();
        assert_eq!(
            &moov[offset_pos..offset_pos + 4],
            &0x11223344u32.to_be_bytes()
        );
    }

    #[test]
    fn test_patch_chunk_offset_returns_error_when_box_missing() {
        let mut moov = Vec::new();
        moov.extend_from_slice(&16u32.to_be_bytes());
        moov.extend_from_slice(b"moov");
        moov.extend_from_slice(&8u32.to_be_bytes());
        moov.extend_from_slice(b"free");
        let err = patch_chunk_offset(&mut moov, 0x01020304).unwrap_err();
        assert!(format!("{err}").contains("stco/co64"));
    }

    #[test]
    fn test_patch_chunk_offset_supports_co64_entries() {
        let config = Mp4Config {
            frame_rate: 24.0,
            width: 16,
            height: 16,
        };
        let sample_sizes = [100u32, 200u32];
        let sync_samples = [1u32];
        let ctx = MoovBuildCtx {
            config: &config,
            timescale: DEFAULT_MP4_TIMESCALE,
            frame_duration: 3750,
            total_duration: 7500,
            sample_count: 2,
            chunk_offset_box: ChunkOffsetBox::Co64,
        };
        let mut moov = build_moov(&ctx, &sample_sizes, &sync_samples).unwrap();

        let (kind, offset_pos) = locate_chunk_offset_entry(&moov).expect("co64 box should exist");
        assert_eq!(kind, ChunkOffsetBox::Co64);
        assert_eq!(&moov[offset_pos..offset_pos + 8], &[0; 8]);

        let large_offset = u64::from(u32::MAX) + 9;
        patch_chunk_offset(&mut moov, large_offset).unwrap();
        assert_eq!(
            &moov[offset_pos..offset_pos + 8],
            &large_offset.to_be_bytes()
        );
    }

    #[test]
    fn test_write_mp4_emits_stco_for_small_files() {
        let config = Mp4Config {
            frame_rate: 24.0,
            width: 16,
            height: 16,
        };
        let frames = vec![MuxFrame {
            av1_data: vec![0x12, 0x34, 0x56, 0x78],
            is_keyframe: true,
        }];
        let mut output = Vec::new();
        write_mp4(&mut output, &config, &frames).unwrap();

        assert!(output.windows(4).any(|w| w == b"stco"));
        assert!(!output.windows(4).any(|w| w == b"co64"));
    }

    #[test]
    fn test_chunk_offset_box_boundary_transition() {
        assert_eq!(
            chunk_offset_box_for_data_offset(u64::from(u32::MAX)),
            ChunkOffsetBox::Stco
        );
        assert_eq!(
            chunk_offset_box_for_data_offset(u64::from(u32::MAX) + 1),
            ChunkOffsetBox::Co64
        );

        let moov_len_at_limit = u32::MAX - FTYP_SIZE - 8;
        let at_limit = compute_data_offset(moov_len_at_limit, 8).unwrap();
        assert_eq!(at_limit, u64::from(u32::MAX));
        assert_eq!(
            chunk_offset_box_for_data_offset(at_limit),
            ChunkOffsetBox::Stco
        );

        let past_limit = compute_data_offset(moov_len_at_limit, 16).unwrap();
        assert_eq!(past_limit, u64::from(u32::MAX) + 8);
        assert_eq!(
            chunk_offset_box_for_data_offset(past_limit),
            ChunkOffsetBox::Co64
        );
    }

    #[test]
    fn test_sample_table_bounds_rejects_oversized_counts() {
        assert!(ensure_sample_table_bounds(MAX_STSZ_SAMPLE_ENTRIES).is_ok());
        assert!(ensure_sample_table_bounds(MAX_STSZ_SAMPLE_ENTRIES + 1).is_err());
    }

    #[test]
    fn test_write_mdat_header_uses_classic_32bit_box_when_payload_fits() {
        let mut bytes = Vec::new();
        write_mdat_header(&mut bytes, 4).unwrap();
        assert_eq!(bytes.len(), 8);
        assert_eq!(u32::from_be_bytes(bytes[0..4].try_into().unwrap()), 12);
        assert_eq!(&bytes[4..8], b"mdat");
    }

    #[test]
    fn test_write_mdat_header_uses_largesize_when_payload_exceeds_u32_box() {
        let payload_len = MAX_MDAT_U32_PAYLOAD + 1;
        let mut bytes = Vec::new();
        write_mdat_header(&mut bytes, payload_len).unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(u32::from_be_bytes(bytes[0..4].try_into().unwrap()), 1);
        assert_eq!(&bytes[4..8], b"mdat");
        assert_eq!(
            u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
            payload_len + 16
        );
    }

    #[test]
    fn test_build_stts_supports_multiple_duration_runs() {
        let stts = build_stts(&[(3, 1000), (2, 1500)]).unwrap();
        assert_eq!(u32::from_be_bytes(stts[0..4].try_into().unwrap()), 32);
        assert_eq!(&stts[4..8], b"stts");
        assert_eq!(u32::from_be_bytes(stts[12..16].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(stts[16..20].try_into().unwrap()), 3);
        assert_eq!(u32::from_be_bytes(stts[20..24].try_into().unwrap()), 1000);
        assert_eq!(u32::from_be_bytes(stts[24..28].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(stts[28..32].try_into().unwrap()), 1500);
    }

    /// Regression test for M-6: the MP4 mux now writes a `colr nclx` box
    /// signalling BT.2020 + ST 2084 + BT.2020 NCL with full range, so MP4
    /// players that read HDR characteristics from the container (rather than
    /// the AV1 OBU) get the correct interpretation.
    #[test]
    fn test_write_mp4_emits_colr_nclx_box() {
        let config = Mp4Config {
            frame_rate: 24.0,
            width: 16,
            height: 16,
        };
        let frames = vec![MuxFrame {
            av1_data: vec![0x00, 0x01, 0x02, 0x03],
            is_keyframe: true,
        }];
        let mut output = Vec::new();
        write_mp4(&mut output, &config, &frames).unwrap();

        // Locate the colr box and verify the nclx tuple.
        let colr_pos = output
            .windows(4)
            .position(|w| w == b"colr")
            .expect("colr box not emitted");
        // colr begins 4 bytes before the four-cc (the u32 size field).
        let nclx_start = colr_pos + 4;
        assert_eq!(&output[nclx_start..nclx_start + 4], b"nclx");
        let prim = u16::from_be_bytes(output[nclx_start + 4..nclx_start + 6].try_into().unwrap());
        let trc = u16::from_be_bytes(output[nclx_start + 6..nclx_start + 8].try_into().unwrap());
        let matrix =
            u16::from_be_bytes(output[nclx_start + 8..nclx_start + 10].try_into().unwrap());
        let full_range_byte = output[nclx_start + 10];
        assert_eq!(prim, 9, "expected BT.2020 primaries");
        assert_eq!(trc, 16, "expected SMPTE ST 2084 transfer");
        assert_eq!(matrix, 9, "expected BT.2020 NCL matrix");
        assert_eq!(full_range_byte & 0x80, 0x80, "expected full_range_flag=1");
    }
}
