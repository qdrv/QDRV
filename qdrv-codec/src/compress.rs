// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Lossless compression for the QDRV mastering tier.
//!
//! QDRV provides two mastering-tier lossless codecs:
//!
//! | Codec | Dependencies | Compression | Speed | Notes |
//! |-------|--------------|-------------|-------|-------|
//! | **fpzip** (default) | None — pure Rust | High | Fast | Designed for Float64 arrays. Exploits inter-channel correlation via `nf=3`. |
//! | **ZFP reversible** (optional) | libzfp via C FFI | Highest | Moderate | Best ratio for 2D spatially-correlated image data. Enable with `--features zfp`. |
//!
//! ## Why not zstd?
//!
//! zstd is an excellent general-purpose compressor but is not aware of the
//! structure of IEEE 754 floating-point data. fpzip and ZFP both exploit the
//! specific bit-layout of floating-point values (sign, exponent, mantissa) and
//! the spatial correlation of adjacent pixels in an image to achieve
//! significantly better compression ratios on floating-point image data.
//!
//! ## Why not SZ3?
//!
//! SZ3 is primarily a **lossy** compressor. QDRV mastering frames require
//! bit-for-bit exact preservation of every Float64 value. SZ3's lossless mode
//! offers no advantage over fpzip or ZFP for smooth image data, so it is not
//! included.
//!
//! ## Per-blob codec identifier byte
//!
//! Each compressed mastering blob begins with a 1-byte codec identifier
//! so the decompressor can dispatch correctly without out-of-band information:
//!
//! | Byte | Codec |
//! |------|-------|
//! | 0 | fpzip (default) |
//! | 1 | ZFP reversible (requires `zfp` feature) |

use crate::error::CodecError;
use qdrv_core::pixel::Pixel64;

// ---------------------------------------------------------------------------
// Codec identifier bytes embedded at the start of each mastering blob
// ---------------------------------------------------------------------------

/// Codec identifier byte for fpzip. The default mastering codec.
pub const MASTERING_CODEC_FPZIP: u8 = 0;

/// Codec identifier byte for ZFP reversible mode.
/// Only written when the `zfp` feature is enabled.
pub const MASTERING_CODEC_ZFP: u8 = 1;

#[cfg(feature = "zfp")]
fn zfp_err(message: impl Into<String>) -> CodecError {
    CodecError::Zfp(message.into())
}

#[cfg(feature = "zfp")]
fn to_u32_len(len: usize, label: &'static str) -> Result<u32, CodecError> {
    u32::try_from(len).map_err(|_| zfp_err(format!("{label} exceeds u32::MAX bytes")))
}

#[cfg(feature = "zfp")]
fn checked_add_usize(a: usize, b: usize, label: &'static str) -> Result<usize, CodecError> {
    a.checked_add(b)
        .ok_or_else(|| zfp_err(format!("{label} overflow")))
}

#[cfg(feature = "zfp")]
fn read_u32_le(payload: &[u8], cursor: &mut usize, label: &'static str) -> Result<u32, CodecError> {
    let end = cursor
        .checked_add(4)
        .ok_or_else(|| zfp_err(format!("{label} offset overflow")))?;
    let bytes = payload
        .get(*cursor..end)
        .ok_or_else(|| zfp_err(format!("truncated {label}")))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(bytes);
    *cursor = end;
    Ok(u32::from_le_bytes(raw))
}

// ---------------------------------------------------------------------------
// MasteringCodec enum
// ---------------------------------------------------------------------------

/// The lossless compression codec to use for a QDRV mastering-tier frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum MasteringCodec {
    /// fpzip: pure Rust, no C dependencies. The default mastering codec.
    /// Achieves good compression ratios on smooth floating-point image data
    /// by exploiting both spatial correlation and inter-channel (R/G/B)
    /// correlation.
    #[default]
    Fpzip,

    /// ZFP reversible mode: highest compression ratio for 2D/3D arrays with
    /// spatial correlation. Requires the `zfp` Cargo feature and a C compiler.
    #[cfg(feature = "zfp")]
    Zfp,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compresses one QDRV mastering-tier frame losslessly.
///
/// The output blob begins with a 1-byte codec identifier (see module docs)
/// followed by the codec-specific compressed bytes. This allows
/// [`decompress_frame`] to decode without requiring out-of-band codec
/// information.
///
/// # Arguments
/// * `pixels` — Float64 linear light pixel buffer in nits (Rec. 2100).
/// * `width`  — Frame width in pixels.
/// * `height` — Frame height in pixels.
/// * `codec`  — Lossless mastering codec to use. Default: [`MasteringCodec::Fpzip`].
///
/// # Errors
/// Returns [`CodecError::Fpzip`] or [`CodecError::Zfp`] on failure.
pub fn compress_frame(
    pixels: &[Pixel64],
    width: u32,
    height: u32,
    codec: MasteringCodec,
) -> Result<Vec<u8>, CodecError> {
    match codec {
        MasteringCodec::Fpzip => compress_fpzip(pixels, width, height),
        #[cfg(feature = "zfp")]
        MasteringCodec::Zfp => compress_zfp(pixels, width, height),
    }
}

/// Decompresses a QDRV mastering-tier frame blob produced by [`compress_frame`].
///
/// The first byte of `data` is the codec identifier. The decoder dispatches
/// based on this byte, so the caller does not need to track which codec was
/// used to write the blob.
///
/// # Errors
/// Returns [`CodecError::UnknownMasteringCodec`] for an unrecognised
/// identifier byte.
pub fn decompress_frame(data: &[u8], expected_pixels: usize) -> Result<Vec<Pixel64>, CodecError> {
    if data.is_empty() {
        return Err(CodecError::MalformedMasteringFrame {
            byte_count: 0,
            bytes_per_pixel: 24,
            expected_pixels,
        });
    }

    match data[0] {
        MASTERING_CODEC_FPZIP => decompress_fpzip(&data[1..], expected_pixels),
        #[cfg(feature = "zfp")]
        MASTERING_CODEC_ZFP => decompress_zfp(&data[1..], expected_pixels),
        other => Err(CodecError::UnknownMasteringCodec(other)),
    }
}

// ---------------------------------------------------------------------------
// fpzip — default mastering codec
// ---------------------------------------------------------------------------

/// Compresses mastering-tier pixels using fpzip lossless floating-point
/// compression.
///
/// The frame is described to fpzip as a 2D array of `width × height` spatial
/// locations with `nf=3` fields per location (the three colour channels R, G,
/// B). This allows fpzip to exploit inter-channel correlations between colour
/// channels at each pixel, typically improving compression ratio over treating
/// the three planes independently.
///
/// The free function `fpzip_rs::compress_f64(data, nx, ny, nz, nf)` is used
/// directly rather than the builder API, as the `nf` parameter is clearly
/// specified on the free function signature.
fn compress_fpzip(pixels: &[Pixel64], width: u32, height: u32) -> Result<Vec<u8>, CodecError> {
    use fpzip_rs::compress_f64;

    // Interleave channels: R₀ G₀ B₀  R₁ G₁ B₁ …
    // With nf=3, fpzip treats each consecutive triple as a vector at one
    // spatial location and exploits inter-channel correlations.
    //
    // U-1: prefer try_reserve_exact over with_capacity for consistency
    // with the rest of the codec; `pixels.len() * 3` could overflow usize
    // for an enormous in-bound input.
    let scalar_count = pixels
        .len()
        .checked_mul(3)
        .ok_or_else(|| CodecError::Fpzip("pixel count × 3 channels overflows usize".to_string()))?;
    let mut raw: Vec<f64> = Vec::new();
    raw.try_reserve_exact(scalar_count).map_err(|_| {
        CodecError::Fpzip(format!(
            "interleaved channel buffer allocation failed for {scalar_count} scalars"
        ))
    })?;
    for p in pixels {
        raw.push(p.r);
        raw.push(p.g);
        raw.push(p.b);
    }

    // nx=width, ny=height, nz=1, nf=3 (three colour channels per pixel).
    let compressed = compress_f64(&raw, width, height, 1, 3)
        .map_err(|e| CodecError::Fpzip(format!("compress failed: {e}")))?;

    let mut out = Vec::with_capacity(1 + compressed.len());
    out.push(MASTERING_CODEC_FPZIP);
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// Cap on the compressed-payload size relative to the expected raw
/// `Pixel64` byte count. fpzip cannot legitimately expand input beyond a
/// small multiple of the uncompressed size for normal data; this bound is
/// generous (matches the reader's 8× compressed-frame budget) but stops a
/// caller from handing a multi-GB payload to the library when the declared
/// frame is tiny. N-4 follow-up.
const FPZIP_INPUT_BUDGET_RATIO: usize = 8;
const FPZIP_BYTES_PER_PIXEL: usize = 24; // f64 × 3 channels
/// Absolute floor so trivially small frames (e.g. 1×1) still allow the
/// fpzip header and a small payload through.
const FPZIP_MIN_INPUT_BUDGET_BYTES: usize = 256 * 1024;

/// Decompresses a fpzip-compressed mastering frame blob.
///
/// Validates `payload.len()` against an `expected_pixels`-derived budget
/// **before** handing the data to the library so we don't ask `fpzip-rs`
/// to allocate working memory for an attacker-supplied multi-GB blob that
/// our wrapper would only reject afterwards on pixel-count mismatch.
fn decompress_fpzip(payload: &[u8], expected_pixels: usize) -> Result<Vec<Pixel64>, CodecError> {
    use fpzip_rs::decompress_f64;

    // Pre-validate the input length so we never call into fpzip-rs with a
    // payload that obviously cannot decode to `expected_pixels` pixels.
    // Overflow in the budget computation is treated as "no cap" only after
    // also enforcing the absolute floor.
    let raw_bytes = expected_pixels.saturating_mul(FPZIP_BYTES_PER_PIXEL);
    let budget = raw_bytes
        .saturating_mul(FPZIP_INPUT_BUDGET_RATIO)
        .max(FPZIP_MIN_INPUT_BUDGET_BYTES);
    if payload.len() > budget {
        return Err(CodecError::Fpzip(format!(
            "compressed payload {} bytes exceeds budget {budget} bytes \
             for {expected_pixels} expected pixels",
            payload.len()
        )));
    }

    let raw = decompress_f64(payload)
        .map_err(|e| CodecError::Fpzip(format!("decompress failed: {e}")))?;

    // P3-2: prefer checked arithmetic so a library user passing an
    // `expected_pixels` near `usize::MAX` cannot trigger debug-mode panic
    // or release-mode wraparound on the scalar-count computation. The
    // reader caps `expected_pixels` long before this point, but defensive
    // bounds here keep the codec usable from external callers too.
    let expected_scalars = expected_pixels.checked_mul(3).ok_or_else(|| {
        CodecError::Fpzip(format!(
            "expected_pixels {expected_pixels} overflows usize when multiplied by 3 channels"
        ))
    })?;
    if raw.len() != expected_scalars {
        return Err(CodecError::PixelCountMismatch {
            expected: expected_scalars,
            actual: raw.len(),
        });
    }

    // Audit M-1 (`AUDIT_REPORT.md` 2026-05-27): validate at the untrusted-input
    // boundary so a corrupted or hand-crafted `.qdrv64` payload that
    // decompresses to NaN/Inf is rejected before the values flow into a
    // user-visible `Vec<Pixel64>`. Switching from `new_unchecked` to `new`
    // costs one finiteness check per channel — negligible vs. the fpzip
    // decode itself.
    raw.chunks_exact(3)
        .enumerate()
        .map(|(idx, c)| {
            Pixel64::new(c[0], c[1], c[2]).map_err(|e| {
                CodecError::Fpzip(format!(
                    "non-finite channel in decompressed mastering frame at pixel {idx}: {e}"
                ))
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ZFP reversible mode — optional feature
// ---------------------------------------------------------------------------

/// Compresses a mastering-tier frame using ZFP reversible (lossless) mode.
///
/// The three colour channels are compressed as separate 2D scalar fields
/// since ZFP operates on scalar arrays rather than vector fields. Each
/// compressed channel blob is preceded by a `u32 LE` byte count. The frame
/// dimensions are stored at the start of the blob so the decompressor can
/// reconstruct the 2D field description (required by ZFP).
///
/// Blob layout:
/// ```text
/// [codec_byte=1]
/// [width: u32 LE] [height: u32 LE]
/// [R_len: u32 LE] [R: ZFP blob]
/// [G_len: u32 LE] [G: ZFP blob]
/// [B_len: u32 LE] [B: ZFP blob]
/// ```
#[cfg(feature = "zfp")]
fn compress_zfp(pixels: &[Pixel64], width: u32, height: u32) -> Result<Vec<u8>, CodecError> {
    let nx = width as usize;
    let ny = height as usize;

    let mut r_plane: Vec<f64> = Vec::with_capacity(pixels.len());
    let mut g_plane: Vec<f64> = Vec::with_capacity(pixels.len());
    let mut b_plane: Vec<f64> = Vec::with_capacity(pixels.len());
    for p in pixels {
        r_plane.push(p.r);
        g_plane.push(p.g);
        b_plane.push(p.b);
    }

    let r_bytes = zfp_compress_plane_f64(&r_plane, nx, ny)?;
    let g_bytes = zfp_compress_plane_f64(&g_plane, nx, ny)?;
    let b_bytes = zfp_compress_plane_f64(&b_plane, nx, ny)?;
    let r_len = to_u32_len(r_bytes.len(), "R plane payload")?;
    let g_len = to_u32_len(g_bytes.len(), "G plane payload")?;
    let b_len = to_u32_len(b_bytes.len(), "B plane payload")?;

    let total = checked_add_usize(
        checked_add_usize(
            checked_add_usize(
                checked_add_usize(1, 8, "ZFP header size")?,
                12,
                "ZFP header size",
            )?,
            r_bytes.len(),
            "ZFP frame size",
        )?,
        checked_add_usize(g_bytes.len(), b_bytes.len(), "ZFP frame size")?,
        "ZFP frame size",
    )?;
    let mut out = Vec::with_capacity(total);
    out.push(MASTERING_CODEC_ZFP);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&r_len.to_le_bytes());
    out.extend_from_slice(&r_bytes);
    out.extend_from_slice(&g_len.to_le_bytes());
    out.extend_from_slice(&g_bytes);
    out.extend_from_slice(&b_len.to_le_bytes());
    out.extend_from_slice(&b_bytes);
    Ok(out)
}

/// Cap on the compressed-payload size of a single ZFP plane relative to its
/// expected raw f64 byte count. ZFP reversible mode is bit-exact and cannot
/// legitimately expand input beyond a small multiple of the uncompressed
/// size; this bound is generous (8×, matching the fpzip budget ratio) but
/// stops an attacker-supplied multi-GB plane payload from forcing the
/// `to_vec()` clone inside `zfp_decompress_plane_f64` to allocate alongside
/// the decoded `Vec<f64>` output. Audit finding M-01
/// (`AUDIT_RUST_WORKSPACE_2026-05-27.md`).
#[cfg(feature = "zfp")]
const ZFP_INPUT_BUDGET_RATIO: usize = 8;
#[cfg(feature = "zfp")]
const ZFP_BYTES_PER_PLANE_PIXEL: usize = 8; // single f64 channel
/// Absolute floor so trivially small planes (e.g. 1×1) still allow the
/// ZFP per-plane header and a small payload through the budget check.
#[cfg(feature = "zfp")]
const ZFP_MIN_INPUT_BUDGET_BYTES: usize = 256 * 1024;

#[cfg(feature = "zfp")]
fn decompress_zfp(payload: &[u8], expected_pixels: usize) -> Result<Vec<Pixel64>, CodecError> {
    if payload.len() < 8 {
        return Err(zfp_err("truncated ZFP blob: missing dimensions"));
    }

    let mut cursor = 0usize;
    let width = read_u32_le(payload, &mut cursor, "ZFP width")? as usize;
    let height = read_u32_le(payload, &mut cursor, "ZFP height")? as usize;

    let plane_pixels = width
        .checked_mul(height)
        .ok_or_else(|| zfp_err("ZFP blob dimensions overflow usize"))?;
    if plane_pixels != expected_pixels {
        return Err(CodecError::PixelCountMismatch {
            expected: expected_pixels,
            actual: plane_pixels,
        });
    }

    // Pre-compute the per-plane compressed-input budget once so each of the
    // three reads share the same bound. M-01: bounds the cloned `buf` and
    // decoded `output` allocations inside `zfp_decompress_plane_f64` against
    // hostile compressed payload sizes that would otherwise inflate transient
    // memory use to multiple GB.
    let raw_plane_bytes = plane_pixels.saturating_mul(ZFP_BYTES_PER_PLANE_PIXEL);
    let plane_budget = raw_plane_bytes
        .saturating_mul(ZFP_INPUT_BUDGET_RATIO)
        .max(ZFP_MIN_INPUT_BUDGET_BYTES);

    let read_plane = |cursor: &mut usize| -> Result<Vec<f64>, CodecError> {
        let len = read_u32_le(payload, cursor, "plane length prefix")? as usize;
        if len > plane_budget {
            return Err(CodecError::Zfp(format!(
                "compressed plane {len} bytes exceeds budget {plane_budget} bytes \
                 for {plane_pixels} expected pixels"
            )));
        }
        let end = cursor
            .checked_add(len)
            .ok_or_else(|| zfp_err("plane byte range overflow"))?;
        let plane = payload
            .get(*cursor..end)
            .ok_or_else(|| zfp_err("truncated plane data"))?;
        *cursor = end;
        zfp_decompress_plane_f64(plane, width, height)
    };

    let r_plane = read_plane(&mut cursor)?;
    let g_plane = read_plane(&mut cursor)?;
    let b_plane = read_plane(&mut cursor)?;

    for (_name, plane) in [("R", &r_plane), ("G", &g_plane), ("B", &b_plane)] {
        if plane.len() != expected_pixels {
            return Err(CodecError::PixelCountMismatch {
                expected: expected_pixels,
                actual: plane.len(),
            });
        }
    }
    if cursor != payload.len() {
        return Err(zfp_err("trailing bytes after ZFP plane data"));
    }

    // Audit M-1 (`AUDIT_REPORT.md` 2026-05-27): same defence-in-depth rule as
    // `decompress_fpzip` — reject any NaN/Inf produced by a corrupted or
    // hand-crafted ZFP payload at the decode boundary rather than letting
    // the values flow into downstream `Vec<Pixel64>` consumers.
    (0..expected_pixels)
        .map(|i| {
            Pixel64::new(r_plane[i], g_plane[i], b_plane[i]).map_err(|e| {
                CodecError::Zfp(format!(
                    "non-finite channel in decompressed ZFP mastering frame at pixel {i}: {e}"
                ))
            })
        })
        .collect()
}

/// Compresses a single f64 plane using ZFP reversible (lossless) mode.
#[cfg(feature = "zfp")]
fn zfp_compress_plane_f64(data: &[f64], nx: usize, ny: usize) -> Result<Vec<u8>, CodecError> {
    use std::ptr;
    use zfp_sys_cc::*;

    // SAFETY: every `zfp_*` call below is guarded by a null check on the
    // returned handle; on failure we free what we have and return early. The
    // pointer cast on `data.as_ptr() as *mut _` is forced by the C API
    // (`zfp_field_2d` takes a `void*` data argument that is *read* during
    // `zfp_compress`, never written, but the signature is non-const). The
    // `bufsize` returned by `zfp_stream_maximum_size` upper-bounds writes
    // into `buf`, so the slice-backed bit-stream cannot overflow. All
    // handles are paired with their matching `*_close` / `*_free` on every
    // exit path, including the early-error returns below.
    unsafe {
        let data_type = zfp_type_zfp_type_double;
        // The `as *mut _` cast is required by the C signature but is
        // sound here because `zfp_compress` only reads from this buffer.
        let field = zfp_field_2d(data.as_ptr() as *mut _, data_type, nx, ny);
        if field.is_null() {
            return Err(CodecError::Zfp("zfp_field_2d returned null".to_string()));
        }
        let zfp = zfp_stream_open(ptr::null_mut());
        if zfp.is_null() {
            zfp_field_free(field);
            return Err(CodecError::Zfp("zfp_stream_open returned null".to_string()));
        }
        zfp_stream_set_reversible(zfp);

        let bufsize = zfp_stream_maximum_size(zfp, field);
        let mut buf: Vec<u8> = vec![0u8; bufsize];
        let stream = stream_open(buf.as_mut_ptr() as *mut _, bufsize);
        if stream.is_null() {
            zfp_stream_close(zfp);
            zfp_field_free(field);
            return Err(CodecError::Zfp("stream_open returned null".to_string()));
        }
        zfp_stream_set_bit_stream(zfp, stream);
        zfp_stream_rewind(zfp);

        let n = zfp_compress(zfp, field);
        stream_flush(stream);
        stream_close(stream);
        zfp_stream_close(zfp);
        zfp_field_free(field);

        if n == 0 {
            return Err(CodecError::Zfp("zfp_compress returned 0 bytes".to_string()));
        }
        buf.truncate(n);
        Ok(buf)
    }
}

/// Decompresses a single f64 plane compressed with ZFP reversible mode.
///
/// The 2D field dimensions must match those used during compression.
#[cfg(feature = "zfp")]
fn zfp_decompress_plane_f64(data: &[u8], nx: usize, ny: usize) -> Result<Vec<f64>, CodecError> {
    use std::ptr;
    use zfp_sys_cc::*;

    let expected_pixels = nx
        .checked_mul(ny)
        .ok_or_else(|| CodecError::Zfp(format!("dimensions {nx}x{ny} overflow usize")))?;
    let mut output: Vec<f64> = vec![0.0f64; expected_pixels];

    // SAFETY: `output` is pre-sized to exactly `nx * ny` f64 slots, matching
    // the field geometry passed to `zfp_field_2d`. `zfp_decompress` writes
    // into that buffer through `field` and cannot exceed those bounds for a
    // well-formed bit-stream. The cloned `buf` (see the M-8 doc comment
    // above) owns the bit-stream memory for the duration of the call; both
    // `stream`, `zfp`, and `field` are released on every exit path
    // (including the null-handle early returns).
    unsafe {
        let data_type = zfp_type_zfp_type_double;
        let field = zfp_field_2d(output.as_mut_ptr() as *mut _, data_type, nx, ny);
        if field.is_null() {
            return Err(CodecError::Zfp("zfp_field_2d returned null".to_string()));
        }
        let zfp = zfp_stream_open(ptr::null_mut());
        if zfp.is_null() {
            zfp_field_free(field);
            return Err(CodecError::Zfp("zfp_stream_open returned null".to_string()));
        }
        zfp_stream_set_reversible(zfp);

        // ZFP's `stream_open` requires a mutable byte buffer because the
        // underlying bit-stream cursor is mutated during decompression. We
        // therefore clone the borrowed `&[u8]` input once per plane; doubles
        // memory pressure at the maximum supported plane size but is the
        // only safe option with the current zfp-sys-cc API.
        let mut buf = data.to_vec();
        let stream = stream_open(buf.as_mut_ptr() as *mut _, buf.len());
        if stream.is_null() {
            zfp_stream_close(zfp);
            zfp_field_free(field);
            return Err(CodecError::Zfp("stream_open returned null".to_string()));
        }
        zfp_stream_set_bit_stream(zfp, stream);
        zfp_stream_rewind(zfp);

        let result = zfp_decompress(zfp, field);
        stream_close(stream);
        zfp_stream_close(zfp);
        zfp_field_free(field);

        if result == 0 {
            return Err(CodecError::Zfp("zfp_decompress returned 0".to_string()));
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pixels_2x2() -> Vec<Pixel64> {
        vec![
            Pixel64::new_unchecked(1000.0, 500.0, 200.0),
            Pixel64::new_unchecked(0.0, 0.0, 0.0),
            Pixel64::new_unchecked(10_000.0, 9_999.99, 0.001),
            Pixel64::new_unchecked(203.0, 203.0, 203.0),
        ]
    }

    #[test]
    fn test_fpzip_lossless_roundtrip() {
        // fpzip must preserve every Float64 value exactly (lossless).
        let pixels = test_pixels_2x2();
        let compressed = compress_frame(&pixels, 2, 2, MasteringCodec::Fpzip).unwrap();
        let recovered = decompress_frame(&compressed, pixels.len()).unwrap();
        for (orig, rec) in pixels.iter().zip(recovered.iter()) {
            assert_eq!(orig.r, rec.r, "R channel not exact");
            assert_eq!(orig.g, rec.g, "G channel not exact");
            assert_eq!(orig.b, rec.b, "B channel not exact");
        }
    }

    #[test]
    fn test_fpzip_above_pq_ceiling_preserved() {
        // Luminance values above 10 000 nits must survive fpzip roundtrip exactly.
        // This is a core QDRV requirement: no existing HDR format can preserve
        // above-ceiling luminance — QDRV mastering can.
        let pixels = vec![Pixel64::new_unchecked(50_000.0, 25_000.0, 12_500.0)];
        let compressed = compress_frame(&pixels, 1, 1, MasteringCodec::Fpzip).unwrap();
        let recovered = decompress_frame(&compressed, 1).unwrap();
        assert_eq!(recovered[0].r, 50_000.0);
        assert_eq!(recovered[0].g, 25_000.0);
        assert_eq!(recovered[0].b, 12_500.0);
    }

    #[test]
    fn test_fpzip_reduces_size_on_uniform_frame() {
        // A uniform solid-colour frame (maximally compressible) must compress
        // to fewer bytes than the raw Float64 representation.
        let pixels: Vec<Pixel64> = vec![Pixel64::new_unchecked(500.0, 500.0, 500.0); 256];
        let raw_size = pixels.len() * 3 * 8; // 3 channels × 8 bytes
        let compressed = compress_frame(&pixels, 16, 16, MasteringCodec::Fpzip).unwrap();
        assert!(
            compressed.len() < raw_size,
            "fpzip output ({} bytes) not smaller than raw ({raw_size} bytes)",
            compressed.len()
        );
    }

    #[test]
    fn test_codec_byte_dispatches_correctly() {
        // The decoder must dispatch correctly from the embedded codec byte,
        // producing identical pixels regardless of which codec was used.
        let pixels = test_pixels_2x2();
        let blob = compress_frame(&pixels, 2, 2, MasteringCodec::Fpzip).unwrap();
        assert_eq!(
            blob[0], MASTERING_CODEC_FPZIP,
            "wrong codec identifier byte"
        );
        let recovered = decompress_frame(&blob, pixels.len()).unwrap();
        for (orig, rec) in pixels.iter().zip(recovered.iter()) {
            assert_eq!(orig.r, rec.r, "R channel mismatch");
            assert_eq!(orig.g, rec.g, "G channel mismatch");
            assert_eq!(orig.b, rec.b, "B channel mismatch");
        }
    }

    /// Regression test for N-4: an attacker-supplied fpzip blob that
    /// far exceeds the reasonable budget for the declared `expected_pixels`
    /// must be rejected by our pre-validation cap *before* it reaches the
    /// fpzip library, so the library is never asked to allocate working
    /// memory proportional to the malicious input.
    #[test]
    fn test_fpzip_rejects_oversized_payload_for_small_expected_pixels() {
        // Build a real, valid 2-pixel fpzip blob so we can prepend a codec
        // byte and then artificially inflate the payload past the budget.
        let pixels = vec![
            Pixel64::new_unchecked(100.0, 100.0, 100.0),
            Pixel64::new_unchecked(200.0, 200.0, 200.0),
        ];
        let mut blob = compress_frame(&pixels, 2, 1, MasteringCodec::Fpzip).unwrap();
        // 4 MiB of padding far exceeds the 256 KiB floor for 2 pixels.
        blob.resize(blob.len() + 4 * 1024 * 1024, 0u8);
        let err = decompress_frame(&blob, pixels.len()).unwrap_err();
        assert!(
            matches!(err, CodecError::Fpzip(ref m) if m.contains("exceeds budget")),
            "expected pre-validation rejection, got: {err:?}"
        );
    }

    #[test]
    fn test_unknown_codec_byte_errors() {
        // A blob with an unrecognised codec identifier byte must return
        // UnknownMasteringCodec, not silently produce garbage.
        let bad = vec![0xFFu8, 0x00, 0x01, 0x02];
        assert!(matches!(
            decompress_frame(&bad, 1),
            Err(CodecError::UnknownMasteringCodec(0xFF))
        ));
    }

    /// ZFP reversible (lossless) round-trip — only built when the
    /// `zfp` Cargo feature is on. Audit L-08: this gives the optional
    /// ZFP code path real unit coverage rather than relying on the
    /// default-feature CI run never exercising it.
    #[cfg(feature = "zfp")]
    #[test]
    fn test_zfp_lossless_roundtrip() {
        let pixels = test_pixels_2x2();
        let compressed = compress_frame(&pixels, 2, 2, MasteringCodec::Zfp).unwrap();
        // Codec identifier byte must be the ZFP marker, not fpzip's.
        assert_eq!(compressed[0], MASTERING_CODEC_ZFP);
        let recovered = decompress_frame(&compressed, pixels.len()).unwrap();
        assert_eq!(
            pixels.len(),
            recovered.len(),
            "ZFP roundtrip pixel-count mismatch"
        );
        for (orig, rec) in pixels.iter().zip(recovered.iter()) {
            assert_eq!(orig.r, rec.r, "ZFP R channel not exact (lossless mode)");
            assert_eq!(orig.g, rec.g, "ZFP G channel not exact (lossless mode)");
            assert_eq!(orig.b, rec.b, "ZFP B channel not exact (lossless mode)");
        }
    }

    /// ZFP must also preserve above-ceiling luminance exactly (same
    /// invariant as `test_fpzip_above_pq_ceiling_preserved`), since the
    /// `Reversible` ZFP mode is bit-exact lossless. Audit L-08.
    #[cfg(feature = "zfp")]
    #[test]
    fn test_zfp_above_pq_ceiling_preserved() {
        let pixels = vec![Pixel64::new_unchecked(50_000.0, 25_000.0, 12_500.0)];
        let compressed = compress_frame(&pixels, 1, 1, MasteringCodec::Zfp).unwrap();
        let recovered = decompress_frame(&compressed, 1).unwrap();
        assert_eq!(recovered[0].r, 50_000.0);
        assert_eq!(recovered[0].g, 25_000.0);
        assert_eq!(recovered[0].b, 12_500.0);
    }

    /// Audit M-01 regression: a hostile ZFP blob that declares a multi-GB
    /// per-plane compressed length must be rejected by the per-plane budget
    /// check *before* the decoder allocates the cloned `buf` or the
    /// decoded `Vec<f64>` output. Constructing the blob from scratch
    /// (rather than via `compress_frame`) is deliberate — we want to
    /// exercise the budget guard without depending on real ZFP output
    /// shapes.
    #[cfg(feature = "zfp")]
    #[test]
    fn test_zfp_rejects_oversized_plane_payload() {
        // 4×4 frame ⇒ 16 plane pixels ⇒ 128 raw bytes ⇒ budget
        // = max(128 × 8, 256 KiB) = 256 KiB.
        let width: u32 = 4;
        let height: u32 = 4;
        let expected_pixels = (width as usize) * (height as usize);
        let oversize = ZFP_MIN_INPUT_BUDGET_BYTES + 1;

        let mut blob = Vec::with_capacity(1 + 8 + 4 + oversize);
        blob.push(MASTERING_CODEC_ZFP);
        blob.extend_from_slice(&width.to_le_bytes());
        blob.extend_from_slice(&height.to_le_bytes());
        // First plane: length prefix that exceeds the per-plane budget.
        let bad_len: u32 = oversize.try_into().expect("oversize fits in u32");
        blob.extend_from_slice(&bad_len.to_le_bytes());
        // Do NOT extend with `oversize` bytes — the budget check must fire
        // strictly on the declared length, before any slice is taken.

        let err = decompress_frame(&blob, expected_pixels)
            .expect_err("oversized ZFP plane must be rejected");
        match err {
            CodecError::Zfp(msg) => {
                assert!(
                    msg.contains("exceeds budget"),
                    "unexpected ZFP error message: {msg}"
                );
            }
            other => panic!("expected CodecError::Zfp, got {other:?}"),
        }
    }

    /// Audit M-1 (`AUDIT_REPORT.md` 2026-05-27): a fpzip blob whose
    /// decompressed channels contain NaN/Inf must be rejected by
    /// `decompress_frame` rather than producing a `Vec<Pixel64>` carrying
    /// non-finite values. The encoder is intentionally permissive (it
    /// preserves whatever bit pattern the caller passes), so we build the
    /// input through `new_unchecked` and rely on the decoder boundary to
    /// reject what comes back out.
    #[test]
    fn test_fpzip_rejects_non_finite_decompressed_pixels() {
        let pixels = vec![
            Pixel64::new_unchecked(1000.0, 500.0, 200.0),
            Pixel64::new_unchecked(f64::NAN, 100.0, 50.0),
        ];
        let compressed = compress_frame(&pixels, 1, 2, MasteringCodec::Fpzip).unwrap();
        let err = decompress_frame(&compressed, 2)
            .expect_err("fpzip decode must reject non-finite channels");
        match err {
            CodecError::Fpzip(msg) => {
                assert!(
                    msg.contains("non-finite"),
                    "unexpected fpzip error message: {msg}"
                );
            }
            other => panic!("expected CodecError::Fpzip, got {other:?}"),
        }
    }

    /// Audit M-1 (`AUDIT_REPORT.md` 2026-05-27): companion to the fpzip
    /// non-finite test for the ZFP path.
    #[cfg(feature = "zfp")]
    #[test]
    fn test_zfp_rejects_non_finite_decompressed_pixels() {
        let pixels = vec![
            Pixel64::new_unchecked(1000.0, 500.0, 200.0),
            Pixel64::new_unchecked(f64::INFINITY, 100.0, 50.0),
        ];
        let compressed = compress_frame(&pixels, 1, 2, MasteringCodec::Zfp).unwrap();
        let err = decompress_frame(&compressed, 2)
            .expect_err("ZFP decode must reject non-finite channels");
        match err {
            CodecError::Zfp(msg) => {
                assert!(
                    msg.contains("non-finite"),
                    "unexpected ZFP error message: {msg}"
                );
            }
            other => panic!("expected CodecError::Zfp, got {other:?}"),
        }
    }
}
