// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! QDRV CLI tool — `qdrv`.
//!
//! Provides command-line access to QDRV format information, SMPTE ST 2084 PQ
//! conversion utilities, metadata inspection, file writing, and file reading.
//!
//! ## Available subcommands
//!
//! | Subcommand | Description |
//! |------------|-------------|
//! | `info`         | Display QDRV format and standards information. |
//! | `pq`           | Convert between nits and SMPTE ST 2084 PQ signal values. |
//! | `meta-static`  | Print an example static stream metadata JSON block. |
//! | `meta-dynamic` | Print an example per-frame dynamic metadata JSON block. |
//! | `meta-dynamic-v2` | Print an example Open Dynamic Metadata v2 JSON block. |
//! | `object-motion` | Add bounded motion metadata to an `ObjectMeta` JSON document. |
//! | `write-test`   | Write a QDRV test-pattern file. |
//! | `convert`      | Convert a mastering (.qdrv64) file to delivery (.qdrv32). |
//! | `hdr10plus`    | Export HDR10+ basic/advanced/adaptive/gaming profile metadata JSON. |
//! | `inspect`      | Read a QDRV file and print its contents to stdout. |
//! | `mux`          | Mux a `.qdrv32` delivery file into an `.mp4` ISOBMFF container. |
//! | `still`        | Export one QDRV frame as an AVIF still image. |
//! | `export-interop` | Export HDR10/HDR10+/DV-compatible interoperability bundle. |
//! | `aces-export` | Export QDRV frames as an ACES/OpenEXR sequence with RRT/ODT output transforms. |
//! | `manifest-sign` / `manifest-verify` | Sign and verify deterministic metadata manifests. |
//! | `conformance-generate-open` / `conformance-run` | Generate and validate deterministic open conformance corpora. |

mod aces_export;
mod conformance;
mod fidelity_eval;
mod interop_export;

use std::{
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::PathBuf,
    time::Instant,
};

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use qdrv_codec::{
    Av1Config, ChromaSampling420, EncodedPacket, GopConfig, MasteringCodec, TemporalEncoder,
    av1_encode,
};
use qdrv_core::{
    pixel::Pixel64,
    pq::{PQ_MAX_NITS, REFERENCE_WHITE_NITS, nits_to_pq, pq_eotf_f32, pq_oetf_f32, pq_to_nits},
};
use qdrv_decode::{
    RenderPolicy, TargetDisplay, TemporalStateManager, sdr::tone_map_to_sdr,
    tone_map_frame_with_policy_and_state,
};
use qdrv_encode::{EncodeOptions, transcode_frame, transcode_frame_with_options};
use qdrv_io::{
    container::{CONTAINER_VERSION_V1, CONTAINER_VERSION_V2, TIER_DELIVERY, TIER_MASTERING},
    reader::QdrvStreamReader,
    writer::{
        ContainerWriteOptions, DeliveryFrame, MasteringFrame, write_delivery_file_with_options,
        write_mastering_file_with_options,
    },
};
use qdrv_meta::{
    DynamicMeta, FidelityContract, InteropLossReport, MotionKeyframe, ObjectMeta, Precision,
    RegionMotion, StaticMeta, Tier, hdr10plus, interoperability, manifest,
    open_dynamic_v2::{
        AmbientAdaptivePolicy, DisplayAdaptationLayer, DisplayModelClass, GamingProfile,
        InverseToneMappingHint, LocalToneMapGrid, OpenDynamicMetadataV2, TemporalConstraint,
    },
};
use qdrv_mux::{
    AvifConfig, Mp4Config, MuxFrame, write_avif, write_cmaf, write_fmp4, write_ivf, write_mp4,
    write_obu_stream,
};

use crate::{
    aces_export::{AcesExportOptions, AcesExportTargetArg, cmd_aces_export},
    conformance::{OpenVectorsConfig, generate_open_vectors, run_conformance},
    fidelity_eval::{measure_fidelity, vmaf_hdr_approximation_allowed_from_env},
    interop_export::export_interop_bundle,
};

const MAX_TEST_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_TEST_FRAMES: u32 = 10_000;

// ---------------------------------------------------------------------------
// CLI codec selector — maps clap value to MasteringCodec
// ---------------------------------------------------------------------------

/// Mastering-tier lossless compression codec.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum MasteringCodecArg {
    /// fpzip: pure Rust, no C dependencies. Default. Best for Float64 arrays.
    Fpzip,
    /// ZFP reversible mode: highest ratio for 2D/3D spatial data.
    /// Requires the `zfp` Cargo feature.
    Zfp,
}

/// Display model selector for adaptation policy and metadata v2.
///
/// Maps onto [`qdrv_meta::open_dynamic_v2::DisplayModelClass`]. Choice of
/// model influences the per-display bias applied by the delivery tone
/// mapper's adaptation layer.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum DisplayModelArg {
    /// Self-emissive OLED panel.
    Oled,
    /// Edge or full-array backlit LCD.
    Lcd,
    /// MiniLED local-dimming LCD (between OLED and LCD in behaviour).
    Miniled,
    /// Front or rear projector.
    Projector,
}

/// QDRV container version for output file writing.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ContainerVersionArg {
    /// Container v1 — legacy compatibility format. Forces metadata
    /// schema v1; combining v1 with `--metadata-v2` is rejected.
    V1,
    /// Container v2 — current writer default; accepts metadata schema
    /// v1 or v2 in the same container.
    V2,
}

/// Output container or elementary-stream format for `qdrv mux`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MuxFormatArg {
    /// Progressive MP4 (single-`mdat` ISOBMFF). Default; plays as a file in
    /// any AV1-capable player.
    Mp4,
    /// Fragmented MP4: an initialisation segment plus keyframe-aligned media
    /// segments, ready to split for adaptive streaming.
    Fmp4,
    /// CMAF (ISO/IEC 23000-19) — fragmented MP4 with brands accepted by both
    /// MPEG-DASH and HLS packagers.
    Cmaf,
    /// IVF — the minimal AOM test container, for the AV1 reference tooling.
    Ivf,
    /// Raw AV1 OBU elementary stream (no container), for bitstream inspection.
    Obu,
}

/// Motion descriptor emitted by `qdrv object-motion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ObjectMotionKindArg {
    /// Keep the selected region fixed for a bounded frame span.
    Static,
    /// Translate the selected region by a fixed per-frame delta.
    Translate,
    /// Interpolate explicit normalised offsets between keyframes.
    PiecewiseLinear,
}

/// HDR10+ export profile mode CLI selector.
///
/// Mirrors [`qdrv_meta::hdr10plus::Hdr10PlusProfileMode`]; the
/// `From<Hdr10PlusModeArg> for Hdr10PlusProfileMode` conversion below
/// keeps them in lock-step.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Hdr10PlusModeArg {
    /// SMPTE ST 2094-40 basic (10-bit) HDR10+ export — the default.
    Basic,
    /// HDR10+ Advanced (16-bit) export with extended precision fields.
    Advanced,
    /// HDR10+ Adaptive-compatible export — embeds v2 ambient policy.
    Adaptive,
    /// HDR10+ Gaming-compatible export — embeds v2 gaming/temporal controls.
    Gaming,
}

impl From<ContainerVersionArg> for u16 {
    fn from(value: ContainerVersionArg) -> Self {
        match value {
            ContainerVersionArg::V1 => CONTAINER_VERSION_V1,
            ContainerVersionArg::V2 => CONTAINER_VERSION_V2,
        }
    }
}

impl From<DisplayModelArg> for DisplayModelClass {
    fn from(value: DisplayModelArg) -> Self {
        match value {
            DisplayModelArg::Oled => DisplayModelClass::Oled,
            DisplayModelArg::Lcd => DisplayModelClass::Lcd,
            DisplayModelArg::Miniled => DisplayModelClass::MiniLed,
            DisplayModelArg::Projector => DisplayModelClass::Projector,
        }
    }
}

impl From<Hdr10PlusModeArg> for hdr10plus::Hdr10PlusProfileMode {
    fn from(value: Hdr10PlusModeArg) -> Self {
        match value {
            Hdr10PlusModeArg::Basic => hdr10plus::Hdr10PlusProfileMode::Basic,
            Hdr10PlusModeArg::Advanced => hdr10plus::Hdr10PlusProfileMode::Advanced,
            Hdr10PlusModeArg::Adaptive => hdr10plus::Hdr10PlusProfileMode::Adaptive,
            Hdr10PlusModeArg::Gaming => hdr10plus::Hdr10PlusProfileMode::Gaming,
        }
    }
}

/// Built-in default signing key for `qdrv conformance-generate-open` when
/// no `--key`/`QDRV_SIGNING_KEY`/`--key-file` is supplied. Keeps the
/// open-vectors corpus reproducible without forcing operators to manage
/// secrets for what is, by design, public test material.
///
/// # Security note (audit L-09)
///
/// **This key is public.** It exists solely so the documented
/// open-vectors corpus reproduces byte-for-byte across machines that
/// haven't been configured with a private key. **Do not** use this
/// constant — or any manifest produced with it — as evidence of
/// authenticity in production workflows. Production callers must
/// supply their own key via `QDRV_SIGNING_KEY` or `--key-file`; both
/// `manifest-sign`/`manifest-verify` and `conformance-run` require an
/// explicit key for that reason, and the user-facing warning is
/// repeated in the README's "Signing key handling" section.
const CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY: &[u8] = b"qdrv-open-conformance-key";

/// Variant of [`resolve_signing_key`] that returns `default` if neither a
/// CLI/env key nor a key-file is supplied. Used by
/// `conformance-generate-open` so the open-vectors workflow continues to
/// "just work" without arguments while keeping the secrets-via-`ps`
/// concern fixed for explicit keys.
///
/// An empty `--key ""` or `QDRV_SIGNING_KEY=""` (a common artefact of
/// shell scripts that try to "clear" the variable) is treated as unset
/// here, so the documented default fires instead of failing with a
/// confusing "empty key" error — that's the P5-1 follow-up. Operators
/// who genuinely want to override the default must supply a non-empty
/// `--key`/env value or use `--key-file`.
///
/// When the built-in default fires, this function emits a two-line
/// stderr warning so an operator who skipped the docs still sees a
/// visible reminder that the resulting signatures are not authenticity
/// evidence. Audit finding L-01
/// (`AUDIT_RUST_WORKSPACE_2026-05-27.md`); the documented warning in the
/// README's "Signing key handling" section is now mirrored at runtime.
fn resolve_signing_key_or_default(
    key: Option<String>,
    key_file: Option<PathBuf>,
    default: &[u8],
    allow_default: bool,
) -> Result<Vec<u8>, String> {
    let key = key.filter(|k| !k.is_empty());
    if key.is_none() && key_file.is_none() {
        if !allow_default {
            return Err(
                "no signing key supplied; pass --key VALUE, set QDRV_SIGNING_KEY, \
                 use --key-file PATH, or pass --allow-public-default-key to sign \
                 with the built-in public open-conformance key"
                    .to_string(),
            );
        }
        eprintln!("warning: signing with the built-in public open-conformance default key");
        eprintln!(
            "warning: pass --key-file PATH or set QDRV_SIGNING_KEY for production-trustworthy signatures"
        );
        return Ok(default.to_vec());
    }
    resolve_signing_key(key, key_file)
}

/// Resolves the manifest signing key from the user's chosen input. Exactly
/// one of `--key`/`QDRV_SIGNING_KEY` or `--key-file` must produce a value.
///
/// `--key-file` reads the file as **raw bytes** (no UTF-8 requirement) so a
/// random binary key can live on disk directly; a single trailing `\r?\n`
/// is stripped because text editors commonly append one. Returning
/// `Vec<u8>` instead of `String` is the P3-I2 follow-up that decoupled the
/// signing surface from the prior text-only path.
///
/// Error messages distinguish the three failure modes the operator might
/// hit so an empty `--key ""` is no longer reported as "missing" (P3-I1):
/// * both sources supplied → conflict
/// * source supplied but empty value → empty
/// * no source supplied at all → missing
///
/// **Relaxation:** [`resolve_signing_key_or_default`] wraps this function
/// so commands that ship with a built-in default (currently only
/// `conformance-generate-open`) can succeed without any of the three
/// sources. Treat the "must produce a value" contract as enforced only
/// when this function is called directly.
fn resolve_signing_key(key: Option<String>, key_file: Option<PathBuf>) -> Result<Vec<u8>, String> {
    match (key, key_file) {
        (Some(_), Some(_)) => Err(
            "specify only one of --key (or QDRV_SIGNING_KEY env var) and --key-file".to_string(),
        ),
        (Some(k), None) => {
            if k.is_empty() {
                return Err(
                    "--key (or QDRV_SIGNING_KEY) is empty; supply a non-empty signing key"
                        .to_string(),
                );
            }
            Ok(k.into_bytes())
        }
        (None, Some(path)) => {
            let mut bytes = fs::read(&path)
                .map_err(|e| format!("cannot read --key-file '{}': {e}", path.display()))?;
            // Strip a single trailing \r?\n so a file written with a normal
            // text editor doesn't accidentally include the newline in the
            // signing key bytes.
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            if bytes.is_empty() {
                return Err(format!("--key-file '{}' is empty", path.display()));
            }
            Ok(bytes)
        }
        (None, None) => Err(
            "missing signing key: pass --key VALUE, set QDRV_SIGNING_KEY, or use --key-file PATH"
                .to_string(),
        ),
    }
}

/// RAII helper that owns a `.part.<pid>` temporary path. If the guard is
/// dropped without an explicit [`TempFileGuard::commit`] call, the
/// destructor removes the temp file so failed writes don't leave partial
/// outputs behind. Used by `write-test` and `convert` so an error mid-write
/// no longer litters the output directory.
pub(crate) struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        // Infallible by type design: `commit(self)` consumes the guard, so
        // any code holding `&self` cannot have called commit. The .expect
        // exists only to satisfy the `Option` discriminant.
        #[allow(clippy::expect_used)]
        self.path
            .as_ref()
            .expect("TempFileGuard accessed after commit (compile-time prevented)")
    }

    /// Take ownership of the path back from the guard, suppressing the
    /// cleanup that would otherwise run on drop. Call this after a
    /// successful `fs::rename` to the final destination.
    pub(crate) fn commit(mut self) -> PathBuf {
        // Infallible: `new()` is the only constructor and always sets
        // `Some(...)`; `commit(self)` consumes so this can run at most once.
        #[allow(clippy::expect_used)]
        self.path
            .take()
            .expect("TempFileGuard committed twice (compile-time prevented)")
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // Best-effort: ignore errors. If the file does not exist (e.g.,
            // we erred before creating it) this is a no-op.
            let _ = fs::remove_file(&path);
        }
    }
}

/// Builds the `.part.<pid>` side-by-side path used by every CLI command
/// that wants atomic-replace semantics. Centralising the path construction
/// keeps the suffix consistent across `cmd_write_test`, `cmd_convert`,
/// `cmd_mux`, and `cmd_export_interop` (DD-2 / DD-3 / GG-2 follow-ups).
pub(crate) fn part_path(output: &std::path::Path) -> PathBuf {
    let ext = output
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!("{e}.part.{}", std::process::id()))
        .unwrap_or_else(|| format!("part.{}", std::process::id()));
    output.with_extension(ext)
}

/// Writes `data` to `path` atomically through a `.part.<pid>` temporary
/// file. The temp file is removed on any error path via [`TempFileGuard`],
/// and the final rename only fires after a successful `sync_all`. Use this
/// helper for every sidecar / report write so a mid-write failure cannot
/// leave a partial file masquerading as a complete one (audit findings
/// DD-3, FF-3, GG-2, plus the `AUDIT_REPORT.md` 2026-05-27 L-1 follow-up
/// that brings the conformance generator onto the same code path).
pub(crate) fn atomic_write(
    path: &std::path::Path,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let tmp_path = part_path(path);
    let tmp_guard = TempFileGuard::new(tmp_path);
    {
        let file = File::create(tmp_guard.path())
            .map_err(|e| format!("cannot create '{}': {e}", tmp_guard.path().display()))?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(data)
            .map_err(|e| format!("write error to '{}': {e}", tmp_guard.path().display()))?;
        let file = writer
            .into_inner()
            .map_err(|e| format!("flush error for '{}': {e}", tmp_guard.path().display()))?;
        file.sync_all()
            .map_err(|e| format!("fsync error for '{}': {e}", tmp_guard.path().display()))?;
    }
    let tmp_path = tmp_guard.commit();
    fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn resolve_mastering_codec(
    arg: MasteringCodecArg,
) -> Result<MasteringCodec, Box<dyn std::error::Error>> {
    match arg {
        MasteringCodecArg::Fpzip => Ok(MasteringCodec::Fpzip),
        #[cfg(feature = "zfp")]
        MasteringCodecArg::Zfp => Ok(MasteringCodec::Zfp),
        #[cfg(not(feature = "zfp"))]
        MasteringCodecArg::Zfp => Err("ZFP support requires rebuilding with `--features zfp`; \
                 refusing implicit fpzip fallback"
            .into()),
    }
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "qdrv",
    // Use the workspace-inherited Cargo version at build time instead of a
    // hard-coded string so this can't drift from `Cargo.toml` (audit
    // finding BB-2). Resolves to `env!("CARGO_PKG_VERSION")`.
    version,
    about = "QDRV — Quantum Dynamic Range Video CLI tool",
    long_about = "Inspection, conversion, and file utilities for QDRV streams.\n\n\
                  Standards:\n\
                  \x20 ITU-R Rec. 2100 (BT.2100) — HDR picture parameter standard\n\
                  \x20 SMPTE ST 2084             — Perceptual Quantizer (PQ) transfer function\n\
                  \x20 SMPTE ST 2094             — Dynamic metadata framework\n\n\
                  Delivery codec   AV1 12-bit 4:4:4 (rav1e encoder, dav1d decoder)\n\
                  Mastering codecs fpzip (default, pure Rust)\n\
                                   ZFP reversible (optional feature, C FFI)\n\n\
                  Licence: GNU General Public Licence v2.0 or later (GPLv2+)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Display QDRV format and standards information.
    Info,

    /// Convert between nits (cd/m²) and SMPTE ST 2084 PQ signal values.
    Pq {
        /// Convert nits → PQ. Provide a luminance value in nits [0, 10 000].
        #[arg(long, conflicts_with = "pq_to_nits")]
        nits: Option<f64>,

        /// Convert PQ → nits. Provide a normalised PQ signal value [0.0, 1.0].
        #[arg(long = "pq")]
        pq_to_nits: Option<f64>,
    },

    /// Print an example static stream metadata JSON block.
    MetaStatic,

    /// Print an example per-frame dynamic metadata JSON block.
    MetaDynamic,

    /// Print an example Open Dynamic Metadata v2 JSON block.
    MetaDynamicV2,

    /// Add bounded motion metadata to one region in an ObjectMeta JSON document.
    ObjectMotion {
        /// Input ObjectMeta JSON file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output ObjectMeta JSON file.
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,

        /// Region ID to update.
        #[arg(long)]
        region_id: u32,

        /// Motion descriptor to write.
        #[arg(long, value_enum, default_value_t = ObjectMotionKindArg::Translate)]
        kind: ObjectMotionKindArg,

        /// Active span in frames, including the authored keyframe.
        #[arg(long)]
        frame_count: Option<u32>,

        /// Horizontal normalised-coordinate delta per frame for translate mode.
        #[arg(long, default_value_t = 0.0)]
        dx_per_frame: f32,

        /// Vertical normalised-coordinate delta per frame for translate mode.
        #[arg(long, default_value_t = 0.0)]
        dy_per_frame: f32,

        /// Target left edge at the final frame; requires --to-y and --frame-count.
        #[arg(long)]
        to_x: Option<f32>,

        /// Target top edge at the final frame; requires --to-x and --frame-count.
        #[arg(long)]
        to_y: Option<f32>,

        /// Piecewise keyframe as FRAME_DELTA:DX:DY. Repeat for non-linear paths.
        #[arg(long = "keyframe", value_name = "FRAME_DELTA:DX:DY")]
        keyframes: Vec<String>,

        /// Replace an existing motion descriptor on the selected region.
        #[arg(long)]
        overwrite: bool,
    },

    /// Write a QDRV test-pattern file.
    ///
    /// Generates a horizontal nit ramp (0 – 1 000 nits) as either a
    /// delivery-tier .qdrv32 (AV1) or a mastering-tier .qdrv64 (fpzip/ZFP).
    WriteTest {
        /// Output file path (.qdrv32 for delivery, .qdrv64 for mastering).
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,

        /// Frame width in pixels.
        #[arg(long, default_value_t = 256)]
        width: u32,

        /// Frame height in pixels.
        #[arg(long, default_value_t = 64)]
        height: u32,

        /// Number of frames to write.
        #[arg(long, default_value_t = 1)]
        frames: u32,

        /// Write a mastering-tier (.qdrv64) file instead of delivery-tier (.qdrv32).
        #[arg(long)]
        mastering: bool,

        /// AV1 quantiser for delivery files (0 = lossless, 255 = lowest quality).
        #[arg(long, default_value_t = 40)]
        quantizer: usize,

        /// rav1e speed preset for delivery files (0 = slowest/best, 10 = fastest).
        #[arg(long, default_value_t = 6)]
        speed: u8,

        /// Mastering-tier lossless codec. Default: fpzip.
        #[arg(long, value_enum, default_value_t = MasteringCodecArg::Fpzip)]
        mastering_codec: MasteringCodecArg,

        /// Output container version. Defaults to v2; set v1 for compatibility output.
        #[arg(long, value_enum, default_value_t = ContainerVersionArg::V2)]
        container_version: ContainerVersionArg,
    },

    /// Convert a mastering-tier (.qdrv64) file to a delivery-tier (.qdrv32) file.
    ///
    /// Reads Float64 linear light frames, transcodes each through the full QDRV
    /// encode pipeline (PQ encoding + AV1 compression), and writes a delivery
    /// container with ST 2094-based dynamic metadata.
    Convert {
        /// Input mastering-tier (.qdrv64) file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output delivery-tier (.qdrv32) file.
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,

        /// AV1 quantiser (0 = lossless, 255 = lowest quality).
        #[arg(long, default_value_t = 40)]
        quantizer: usize,

        /// rav1e speed preset (0 = slowest/best, 10 = fastest).
        #[arg(long, default_value_t = 6)]
        speed: u8,

        /// Generate an SDR fallback PPM file alongside the .qdrv32 output.
        #[arg(long)]
        sdr: Option<PathBuf>,

        /// Write a simultaneous HDR10-compatible 10-bit raw YUV file.
        #[arg(long)]
        hdr10: Option<PathBuf>,

        /// Enable deterministic render/transcode mode.
        #[arg(long)]
        deterministic: bool,

        /// Enable creator intent locking in generated metadata.
        #[arg(long)]
        creator_intent_lock: bool,

        /// Emit Open Dynamic Metadata v2 payload in output.
        #[arg(long)]
        metadata_v2: bool,

        /// Ambient lux value used for v2 ambient policy tagging.
        #[arg(long)]
        ambient_lux: Option<f32>,

        /// Display model for adaptation layer tagging.
        #[arg(long, value_enum)]
        display_model: Option<DisplayModelArg>,

        /// Frame-time estimate in ms for low-latency gaming profile.
        #[arg(long)]
        frame_time_ms: Option<f32>,

        /// Optional fidelity contract JSON path.
        #[arg(long)]
        fidelity_contract: Option<PathBuf>,

        /// Optional interoperability loss report output path.
        #[arg(long)]
        interop_report: Option<PathBuf>,

        /// Optional DV-compatible sidecar output path.
        #[arg(long)]
        dv_sidecar: Option<PathBuf>,

        /// Output container version. Defaults to v2; set v1 for compatibility output.
        #[arg(long, value_enum, default_value_t = ContainerVersionArg::V2)]
        container_version: ContainerVersionArg,
    },

    /// Export HDR10+ profile metadata JSON from a QDRV file.
    Hdr10plus {
        /// Input .qdrv32 or .qdrv64 file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output HDR10+ JSON file.
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,

        /// Export profile mode: basic, advanced, adaptive, or gaming.
        #[arg(long, value_enum, default_value_t = Hdr10PlusModeArg::Basic)]
        mode: Hdr10PlusModeArg,

        /// Legacy alias for `--mode advanced`.
        ///
        /// Kept for compatibility with existing scripts.
        #[arg(long, conflicts_with = "mode")]
        advanced: bool,
    },

    /// Read a QDRV file and print its header, metadata, and per-frame statistics.
    Inspect {
        /// Path to a .qdrv32 or .qdrv64 file.
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Print the full static metadata JSON block.
        #[arg(long)]
        meta: bool,

        /// Print per-frame dynamic metadata JSON blocks.
        #[arg(long)]
        frames: bool,

        /// Simulate low-latency render with stateful temporal anti-pumping.
        #[arg(long)]
        render_frame_time_ms: Option<f32>,

        /// Target display peak used with --render-frame-time-ms.
        #[arg(long, default_value_t = 1000.0)]
        render_target_max_nits: f32,
    },

    /// Export HDR10/HDR10+/DV-compatible interoperability bundle.
    ExportInterop {
        /// Input .qdrv32 or .qdrv64 file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output directory for artefacts (HDR10 raw, HDR10+ JSON, DV sidecar, reports).
        #[arg(value_name = "OUTPUT_DIR")]
        output_dir: PathBuf,

        /// Optional external proprietary DV adapter command.
        ///
        /// Placeholders:
        /// - {sidecar}: generated open DV-compatible sidecar JSON
        /// - {rpu}: output proprietary RPU bitstream path
        /// - {report}: adapter report JSON path
        #[arg(long)]
        dv_tool_cmd: Option<String>,
    },

    /// Export QDRV frames as an ACES/OpenEXR sequence.
    ///
    /// Reads a delivery-tier `.qdrv32` or mastering-tier `.qdrv64` stream,
    /// converts Rec.2020 absolute luminance into scene-linear ACES AP0 using
    /// `--reference-white-nits`, and writes one OpenEXR file per frame. The
    /// selected `--target` can preserve ACES2065-1 scene-linear AP0 directly
    /// or apply the ACES RRT plus the chosen Rec.709/Rec.2020 ODT.
    AcesExport {
        /// Input .qdrv32 or .qdrv64 file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output directory for the EXR frame sequence.
        #[arg(value_name = "OUTPUT_DIR")]
        output_dir: PathBuf,

        /// Output transform to write.
        #[arg(long, value_enum, default_value_t = AcesExportTargetArg::Rec709100Nit)]
        target: AcesExportTargetArg,

        /// Absolute luminance that maps to scene-linear ACES value 1.0.
        #[arg(long, default_value_t = REFERENCE_WHITE_NITS)]
        reference_white_nits: f64,

        /// File-name prefix; output files are PREFIX_000000.exr, etc.
        #[arg(long, default_value = "frame")]
        prefix: String,

        /// First frame number used in the EXR sequence file names.
        #[arg(long, default_value_t = 0)]
        start_number: u32,
    },

    /// Sign metadata JSON with deterministic manifest.
    ManifestSign {
        /// Input metadata JSON file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,
        /// Output manifest JSON file.
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,
        /// Signing key string. Prefer `QDRV_SIGNING_KEY` env var or
        /// `--key-file` to avoid leaking secrets via `ps`/shell history.
        #[arg(long, env = "QDRV_SIGNING_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Read the signing key from this file's contents (trailing newline
        /// stripped). Preferred over `--key` for production use.
        #[arg(long, conflicts_with = "key")]
        key_file: Option<PathBuf>,
        /// Signer identity.
        #[arg(long, default_value = "qdrv-tool")]
        signer: String,
    },

    /// Verify metadata JSON against a signed manifest.
    ManifestVerify {
        /// Input metadata JSON file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,
        /// Manifest JSON file.
        #[arg(value_name = "MANIFEST")]
        manifest_path: PathBuf,
        /// Signing key string. Prefer `QDRV_SIGNING_KEY` env var or
        /// `--key-file` to avoid leaking secrets via `ps`/shell history.
        #[arg(long, env = "QDRV_SIGNING_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Read the signing key from this file's contents (trailing newline
        /// stripped). Preferred over `--key` for production use.
        #[arg(long, conflicts_with = "key")]
        key_file: Option<PathBuf>,
    },

    /// Mux a delivery-tier QDRV file (`.qdrv32`) into a delivery container or AV1 elementary stream.
    ///
    /// The QDRV file's per-frame pixels are re-encoded as a temporally-predicted
    /// AV1 bitstream (12-bit 4:4:4, ITU-R Rec. 2020 primaries, SMPTE ST 2084 PQ
    /// transfer) and written into a minimal MP4 with one video track. The
    /// resulting `.mp4` is playable by any standards-compliant AV1 player and
    /// carries an HDR `colr` `nclx` box advertising BT.2020 / ST 2084 / BT.2020 NCL.
    Mux {
        /// Input delivery-tier `.qdrv32` file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output file. The container or elementary stream is selected by
        /// `--format` (default `mp4`); pick a matching extension.
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,

        /// Frame rate (frames per second) embedded in the MP4 timing tables.
        #[arg(long, default_value_t = 24.0)]
        frame_rate: f64,

        /// AV1 quantiser for re-encode (0 = lossless, 255 = lowest quality).
        #[arg(long, default_value_t = 40)]
        quantizer: usize,

        /// rav1e speed preset for re-encode (0 = slowest/best, 10 = fastest).
        #[arg(long, default_value_t = 6)]
        speed: u8,

        /// Maximum number of frames between AV1 keyframes (GOP length). Also
        /// the media-segment boundary for the `fmp4`/`cmaf` formats.
        #[arg(long, default_value_t = 120)]
        keyframe_interval: u32,

        /// Output format: `mp4` (progressive, default), `fmp4`/`cmaf`
        /// (fragmented, keyframe-segmented for adaptive streaming), or
        /// `ivf`/`obu` (AV1 elementary streams for codec tooling).
        #[arg(long, value_enum, default_value = "mp4")]
        format: MuxFormatArg,
    },

    /// Export one QDRV frame as a standards-based AVIF still image.
    ///
    /// Accepts delivery-tier `.qdrv32` or mastering-tier `.qdrv64` input,
    /// selects one frame with `--frame-index`, encodes it as an AV1 still
    /// picture, and writes a HEIF/AVIF file with HDR `colr nclx` signalling
    /// plus a QDRV JSON metadata item.
    Still {
        /// Input `.qdrv32` or `.qdrv64` file.
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output `.avif` file.
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,

        /// Zero-based frame index to export.
        #[arg(long, default_value_t = 0)]
        frame_index: u32,

        /// AV1 quantiser for still-image encode (0 = lossless, 255 = lowest quality).
        #[arg(long, default_value_t = 40)]
        quantizer: usize,

        /// rav1e speed preset for still-image encode (0 = slowest/best, 10 = fastest).
        #[arg(long, default_value_t = 6)]
        speed: u8,

        /// Use deterministic mastering-to-delivery quantisation and single-threaded AV1 encode.
        #[arg(long)]
        deterministic: bool,
    },

    /// Read embedded QDRV dynamic metadata back out of an exported stream:
    /// MP4 / fragmented MP4 / CMAF, IVF (`.ivf`), or a raw AV1 OBU (`.obu`).
    ///
    /// QDRV carries per-frame dynamic metadata inside the AV1 bitstream as
    /// ITU-T T.35 metadata OBUs, so it survives any container. This demuxes the
    /// input as needed, extracts those OBUs, and prints a per-frame summary.
    ProbeStream {
        /// Input file: `.mp4` (progressive/fragmented/CMAF), `.ivf`, or raw `.obu`.
        #[arg(value_name = "INPUT")]
        input: PathBuf,
    },

    /// Generate deterministic open conformance vectors + manifest.
    ConformanceGenerateOpen {
        /// Output directory for generated vectors and manifest.
        #[arg(value_name = "OUTPUT_DIR")]
        output_dir: PathBuf,
        /// Corpus label embedded in manifest.
        #[arg(long, default_value = "qdrv-open-vectors")]
        corpus_name: String,
        /// Number of vectors to generate.
        #[arg(long, default_value_t = 3)]
        vectors: usize,
        /// Frame width for generated vectors.
        #[arg(long, default_value_t = 64)]
        width: u32,
        /// Frame height for generated vectors.
        #[arg(long, default_value_t = 64)]
        height: u32,
        /// Deterministic signing key for vector metadata manifests.
        ///
        /// Prefer `QDRV_SIGNING_KEY` or `--key-file` to avoid leaking
        /// secrets via `ps`/shell history. To use the built-in public
        /// open-conformance default key instead, pass
        /// `--allow-public-default-key` — the command will otherwise
        /// fail rather than silently sign with public material.
        #[arg(long, env = "QDRV_SIGNING_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Read the signing key from this file's contents (raw bytes;
        /// trailing newline stripped). Mutually exclusive with `--key`.
        #[arg(long, conflicts_with = "key")]
        key_file: Option<PathBuf>,
        /// Explicitly opt in to signing with the built-in public
        /// open-conformance default key when no `--key`/env value or
        /// `--key-file` is supplied. Required for the reproducible
        /// open-vectors workflow; production callers must supply their
        /// own key instead. Audit LOW
        /// (`AUDIT_REPORT_2026-05-27_2339.md`): replaces the previous
        /// fail-open-with-warning behaviour so the default key can only
        /// fire after explicit operator consent.
        #[arg(long)]
        allow_public_default_key: bool,
        /// Signer identity for metadata manifests.
        #[arg(long, default_value = "qdrv-open-vectors")]
        signer: String,
    },

    /// Run golden corpus conformance checks from a manifest.
    ConformanceRun {
        /// Corpus manifest JSON file.
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
        /// Output directory for candidate renders and summaries.
        #[arg(value_name = "OUTPUT_DIR")]
        output_dir: PathBuf,
        /// Signing key used to verify metadata manifests. Prefer
        /// `QDRV_SIGNING_KEY` or `--key-file` to avoid leaking secrets
        /// via `ps`/shell history.
        #[arg(long, env = "QDRV_SIGNING_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Read the signing key from this file's contents (raw bytes;
        /// trailing newline stripped). Mutually exclusive with `--key`.
        #[arg(long, conflicts_with = "key")]
        key_file: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Info => {
            cmd_info();
            Ok(())
        }
        Commands::Pq {
            nits,
            pq_to_nits: pq,
        } => cmd_pq(nits, pq),
        Commands::MetaStatic => {
            cmd_meta_static();
            Ok(())
        }
        Commands::MetaDynamic => {
            cmd_meta_dynamic();
            Ok(())
        }
        Commands::MetaDynamicV2 => {
            cmd_meta_dynamic_v2();
            Ok(())
        }
        Commands::ObjectMotion {
            input,
            output,
            region_id,
            kind,
            frame_count,
            dx_per_frame,
            dy_per_frame,
            to_x,
            to_y,
            keyframes,
            overwrite,
        } => cmd_object_motion(ObjectMotionOptions {
            input: &input,
            output: &output,
            region_id,
            kind,
            frame_count,
            dx_per_frame,
            dy_per_frame,
            to_x,
            to_y,
            keyframes: &keyframes,
            overwrite,
        }),
        Commands::WriteTest {
            output,
            width,
            height,
            frames,
            mastering,
            quantizer,
            speed,
            mastering_codec,
            container_version,
        } => cmd_write_test(WriteTestOptions {
            output: &output,
            width,
            height,
            frame_count: frames,
            write_mastering: mastering,
            quantizer,
            speed,
            codec_arg: mastering_codec,
            container_version,
        }),
        Commands::Convert {
            input,
            output,
            quantizer,
            speed,
            sdr,
            hdr10,
            deterministic,
            creator_intent_lock,
            metadata_v2,
            ambient_lux,
            display_model,
            frame_time_ms,
            fidelity_contract,
            interop_report,
            dv_sidecar,
            container_version,
        } => cmd_convert(ConvertOptions {
            input: &input,
            output: &output,
            quantizer,
            speed,
            sdr_path: sdr.as_deref(),
            hdr10_path: hdr10.as_deref(),
            deterministic,
            creator_intent_lock,
            metadata_v2,
            ambient_lux,
            display_model,
            frame_time_ms,
            fidelity_contract_path: fidelity_contract.as_deref(),
            interop_report_path: interop_report.as_deref(),
            dv_sidecar_path: dv_sidecar.as_deref(),
            container_version,
        }),
        Commands::Hdr10plus {
            input,
            output,
            mode,
            advanced,
        } => {
            let selected = if advanced {
                Hdr10PlusModeArg::Advanced
            } else {
                mode
            };
            cmd_hdr10plus(&input, &output, selected.into())
        }
        Commands::Inspect {
            file,
            meta,
            frames,
            render_frame_time_ms,
            render_target_max_nits,
        } => cmd_inspect(
            &file,
            meta,
            frames,
            render_frame_time_ms,
            render_target_max_nits,
        ),
        Commands::ExportInterop {
            input,
            output_dir,
            dv_tool_cmd,
        } => cmd_export_interop(&input, &output_dir, dv_tool_cmd.as_deref()),
        Commands::AcesExport {
            input,
            output_dir,
            target,
            reference_white_nits,
            prefix,
            start_number,
        } => cmd_aces_export(AcesExportOptions {
            input: &input,
            output_dir: &output_dir,
            target,
            reference_white_nits,
            prefix: &prefix,
            start_number,
        }),
        Commands::ManifestSign {
            input,
            output,
            key,
            key_file,
            signer,
        } => match resolve_signing_key(key, key_file) {
            Ok(key) => cmd_manifest_sign(&input, &output, &key, &signer),
            Err(e) => Err(e.into()),
        },
        Commands::ManifestVerify {
            input,
            manifest_path,
            key,
            key_file,
        } => match resolve_signing_key(key, key_file) {
            Ok(key) => cmd_manifest_verify(&input, &manifest_path, &key),
            Err(e) => Err(e.into()),
        },
        Commands::Mux {
            input,
            output,
            frame_rate,
            quantizer,
            speed,
            keyframe_interval,
            format,
        } => cmd_mux(
            &input,
            &output,
            frame_rate,
            quantizer,
            speed,
            keyframe_interval,
            format,
        ),
        Commands::Still {
            input,
            output,
            frame_index,
            quantizer,
            speed,
            deterministic,
        } => cmd_still(
            &input,
            &output,
            frame_index,
            quantizer,
            speed,
            deterministic,
        ),
        Commands::ProbeStream { input } => cmd_probe_stream(&input),
        Commands::ConformanceGenerateOpen {
            output_dir,
            corpus_name,
            vectors,
            width,
            height,
            key,
            key_file,
            allow_public_default_key,
            signer,
        } => match resolve_signing_key_or_default(
            key,
            key_file,
            CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY,
            allow_public_default_key,
        ) {
            Ok(key) => cmd_conformance_generate_open(
                &output_dir,
                &corpus_name,
                vectors,
                width,
                height,
                &key,
                &signer,
            ),
            Err(e) => Err(e.into()),
        },
        Commands::ConformanceRun {
            manifest,
            output_dir,
            key,
            key_file,
        } => match resolve_signing_key(key, key_file) {
            Ok(key) => cmd_conformance_run(&manifest, &output_dir, &key),
            Err(e) => Err(e.into()),
        },
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

fn cmd_info() {
    let zfp_status = if cfg!(feature = "zfp") {
        "enabled"
    } else {
        "disabled (build with --features zfp)"
    };

    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│         QDRV — Quantum Dynamic Range Video v0.1.0       │");
    println!("└─────────────────────────────────────────────────────────┘");
    println!();
    println!("  Mastering tier   Float64 linear light, unbounded luminance");
    println!("  Delivery tier    Float32 SMPTE ST 2084 PQ-encoded");
    println!();
    println!("  Colour standard  ITU-R Rec. 2100 (BT.2100)");
    println!("  Colour primaries ITU-R Rec. 2020 (BT.2020)");
    println!("  Transfer fn      SMPTE ST 2084 (Perceptual Quantizer)");
    println!("  Dynamic metadata SMPTE ST 2094-based, Float32");
    println!();
    println!("  Delivery codec   AV1 12-bit 4:4:4 (rav1e + dav1d)");
    println!("  Mastering codecs fpzip  [default] pure Rust, Float64-optimised");
    println!("                   ZFP    [{zfp_status}]");
    println!("  Container IO     read v1/v2, write v2 by default");
    println!();
    println!("  PQ max luminance {PQ_MAX_NITS:.0} nits");
    println!("  Reference white  {REFERENCE_WHITE_NITS:.0} nits (ITU-R BT.2408)");
    println!();
    println!("  Licence          GNU General Public Licence v2.0 or later (GPLv2+)");
    println!("  Status           v0.1.0 Working Draft");
}

// ---------------------------------------------------------------------------
// pq
// ---------------------------------------------------------------------------

fn cmd_pq(nits: Option<f64>, pq: Option<f64>) -> Result<(), Box<dyn std::error::Error>> {
    // Audit LOW (`AUDIT_REPORT_28-05-2026_2053.md`): invalid `--nits`/`--pq`
    // input previously printed to stderr and the command exited 0, so a
    // calling script could not distinguish "valid conversion" from
    // "rejected input". Now both validation errors propagate through the
    // `main` error path and the process exits non-zero.
    match (nits, pq) {
        (Some(n), _) => {
            let v = nits_to_pq(n).map_err(|e| format!("Error: {e}"))?;
            println!("{n:.4} nits  →  PQ {v:.8}");
            Ok(())
        }
        (_, Some(p)) => {
            let v = pq_to_nits(p).map_err(|e| format!("Error: {e}"))?;
            println!("PQ {p:.8}  →  {v:.4} nits");
            Ok(())
        }
        (None, None) => {
            println!("SMPTE ST 2084 PQ reference values:");
            println!("  {:>10}  {:>12}  Note", "Nits", "PQ signal");
            println!("  {:>10}  {:>12}  ----", "----", "---------");
            let entries: &[(f64, &str)] = &[
                (0.0, "absolute black"),
                (0.1, ""),
                (1.0, ""),
                (10.0, ""),
                (100.0, ""),
                (203.0, "reference white (BT.2408)"),
                (400.0, ""),
                (1000.0, "HDR10 mastering ceiling"),
                (4000.0, "common Dolby Vision master"),
                (10000.0, "ST 2084 PQ maximum"),
            ];
            for &(n, note) in entries {
                match nits_to_pq(n) {
                    Ok(pq_val) => println!("  {:>10.1}  {pq_val:>12.8}  {note}", n),
                    Err(e) => eprintln!("  {:>10.1}  <error: {e}>  {note}", n),
                }
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// meta-static / meta-dynamic
// ---------------------------------------------------------------------------

fn cmd_meta_static() {
    let meta = StaticMeta::default_delivery(1000.0, 400.0);
    match qdrv_meta::to_json(&meta) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn cmd_meta_dynamic() {
    let meta = DynamicMeta::new(0, 1200.0, 180.0);
    match qdrv_meta::to_json(&meta) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn cmd_meta_dynamic_v2() {
    let mut meta = DynamicMeta::new(0, 1200.0, 180.0);
    meta.metadata_schema_version = qdrv_meta::compatibility::METADATA_SCHEMA_V2;
    meta.open_dynamic_v2 = Some(sample_open_dynamic_v2(None, None, None));
    meta.inverse_tone_mapping_hint = Some(InverseToneMappingHint::default());
    match qdrv_meta::to_json(&meta) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Error: {e}"),
    }
}

pub(crate) struct ObjectMotionOptions<'a> {
    input: &'a std::path::Path,
    output: &'a std::path::Path,
    region_id: u32,
    kind: ObjectMotionKindArg,
    frame_count: Option<u32>,
    dx_per_frame: f32,
    dy_per_frame: f32,
    to_x: Option<f32>,
    to_y: Option<f32>,
    keyframes: &'a [String],
    overwrite: bool,
}

fn cmd_object_motion(opts: ObjectMotionOptions<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let json = fs::read_to_string(opts.input).map_err(|e| {
        format!(
            "cannot read ObjectMeta JSON '{}': {e}",
            opts.input.display()
        )
    })?;
    let mut meta: ObjectMeta = qdrv_meta::from_json(&json).map_err(|e| {
        format!(
            "cannot parse ObjectMeta JSON '{}': {e}",
            opts.input.display()
        )
    })?;

    let region_index = meta
        .regions
        .iter()
        .position(|region| region.id == opts.region_id)
        .ok_or_else(|| format!("object region id {} was not found", opts.region_id))?;

    if meta.regions[region_index].motion.is_some() && !opts.overwrite {
        return Err(format!(
            "object region id {} already has motion; pass --overwrite to replace it",
            opts.region_id
        )
        .into());
    }

    let base_box = meta.regions[region_index].bounding_box;
    let motion = build_region_motion(&opts, base_box)?;
    meta.regions[region_index].motion = Some(motion);
    meta.validate()
        .map_err(|e| format!("ObjectMeta validation failed after motion update: {e}"))?;

    let out_json =
        qdrv_meta::to_json(&meta).map_err(|e| format!("cannot serialise ObjectMeta JSON: {e}"))?;
    atomic_write(opts.output, out_json.as_bytes())?;
    println!(
        "Wrote object motion metadata for region {}: {}",
        opts.region_id,
        opts.output.display()
    );
    Ok(())
}

fn build_region_motion(
    opts: &ObjectMotionOptions<'_>,
    base_box: qdrv_meta::BoundingBox,
) -> Result<RegionMotion, Box<dyn std::error::Error>> {
    let motion = match opts.kind {
        ObjectMotionKindArg::Static => {
            reject_keyframes(opts)?;
            reject_target_endpoint(opts)?;
            if opts.dx_per_frame != 0.0 || opts.dy_per_frame != 0.0 {
                return Err(
                    "--dx-per-frame/--dy-per-frame are only valid for translate mode".into(),
                );
            }
            RegionMotion::Static {
                frame_count: required_frame_count(opts.frame_count)?,
            }
        }
        ObjectMotionKindArg::Translate => {
            reject_keyframes(opts)?;
            let frame_count = required_frame_count(opts.frame_count)?;
            let (dx_per_frame, dy_per_frame) =
                translate_delta_from_options(opts, base_box, frame_count)?;
            RegionMotion::Translate {
                dx_per_frame,
                dy_per_frame,
                frame_count,
            }
        }
        ObjectMotionKindArg::PiecewiseLinear => {
            if opts.frame_count.is_some() {
                return Err("--frame-count is inferred from piecewise keyframes".into());
            }
            reject_target_endpoint(opts)?;
            if opts.dx_per_frame != 0.0 || opts.dy_per_frame != 0.0 {
                return Err(
                    "--dx-per-frame/--dy-per-frame are only valid for translate mode".into(),
                );
            }
            let keyframes = opts
                .keyframes
                .iter()
                .map(|spec| parse_motion_keyframe(spec))
                .collect::<Result<Vec<_>, _>>()?;
            RegionMotion::PiecewiseLinear { keyframes }
        }
    };

    motion
        .validate_for_box(base_box)
        .map_err(|e| format!("invalid region motion: {e}"))?;
    Ok(motion)
}

fn required_frame_count(frame_count: Option<u32>) -> Result<u32, Box<dyn std::error::Error>> {
    let Some(frame_count) = frame_count else {
        return Err("--frame-count is required for static and translate motion".into());
    };
    if frame_count == 0 {
        return Err("--frame-count must be greater than zero".into());
    }
    Ok(frame_count)
}

fn reject_keyframes(opts: &ObjectMotionOptions<'_>) -> Result<(), Box<dyn std::error::Error>> {
    if !opts.keyframes.is_empty() {
        return Err("--keyframe is only valid for piecewise-linear motion".into());
    }
    Ok(())
}

fn reject_target_endpoint(
    opts: &ObjectMotionOptions<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    if opts.to_x.is_some() || opts.to_y.is_some() {
        return Err("--to-x/--to-y are only valid for translate motion".into());
    }
    Ok(())
}

fn translate_delta_from_options(
    opts: &ObjectMotionOptions<'_>,
    base_box: qdrv_meta::BoundingBox,
    frame_count: u32,
) -> Result<(f32, f32), Box<dyn std::error::Error>> {
    match (opts.to_x, opts.to_y) {
        (Some(to_x), Some(to_y)) => {
            if opts.dx_per_frame != 0.0 || opts.dy_per_frame != 0.0 {
                return Err(
                    "--to-x/--to-y cannot be combined with --dx-per-frame/--dy-per-frame".into(),
                );
            }
            if frame_count < 2 {
                return Err("endpoint interpolation requires --frame-count of at least 2".into());
            }
            let span = (frame_count - 1) as f32;
            Ok(((to_x - base_box.x) / span, (to_y - base_box.y) / span))
        }
        (None, None) => Ok((opts.dx_per_frame, opts.dy_per_frame)),
        _ => Err("--to-x and --to-y must be supplied together".into()),
    }
}

fn parse_motion_keyframe(spec: &str) -> Result<MotionKeyframe, Box<dyn std::error::Error>> {
    let mut parts = spec.split(':');
    let Some(frame_delta) = parts.next() else {
        return Err("keyframe must be FRAME_DELTA:DX:DY".into());
    };
    let Some(dx) = parts.next() else {
        return Err("keyframe must be FRAME_DELTA:DX:DY".into());
    };
    let Some(dy) = parts.next() else {
        return Err("keyframe must be FRAME_DELTA:DX:DY".into());
    };
    if parts.next().is_some() {
        return Err("keyframe must be FRAME_DELTA:DX:DY".into());
    }

    let frame_delta = frame_delta
        .parse::<u32>()
        .map_err(|e| format!("invalid keyframe frame delta in '{spec}': {e}"))?;
    let dx = dx
        .parse::<f32>()
        .map_err(|e| format!("invalid keyframe dx in '{spec}': {e}"))?;
    let dy = dy
        .parse::<f32>()
        .map_err(|e| format!("invalid keyframe dy in '{spec}': {e}"))?;
    if !dx.is_finite() || !dy.is_finite() {
        return Err(format!("keyframe offsets must be finite in '{spec}'").into());
    }
    Ok(MotionKeyframe {
        frame_delta,
        dx,
        dy,
    })
}

// ---------------------------------------------------------------------------
// write-test
// ---------------------------------------------------------------------------

/// Writes a QDRV test-pattern file — either a delivery-tier AV1 file or
/// a mastering-tier losslessly compressed file.
///
/// The test pattern is a horizontal nit ramp: column 0 is 0 nits and the
/// rightmost column is 1 000 nits. For delivery files the mastering ramp is
/// transcoded through the full QDRV encode pipeline (PQ encoding + AV1).
/// For mastering files the Float64 ramp is written directly via the chosen
/// lossless codec.
pub(crate) struct WriteTestOptions<'a> {
    pub output: &'a std::path::Path,
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
    pub write_mastering: bool,
    pub quantizer: usize,
    pub speed: u8,
    pub codec_arg: MasteringCodecArg,
    pub container_version: ContainerVersionArg,
}

fn cmd_write_test(opts: WriteTestOptions<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let WriteTestOptions {
        output,
        width,
        height,
        frame_count,
        write_mastering,
        quantizer,
        speed,
        codec_arg,
        container_version,
    } = opts;
    if width == 0 || height == 0 {
        return Err("width and height must both be greater than zero".into());
    }
    if frame_count == 0 {
        return Err("frame count must be greater than zero".into());
    }
    if frame_count > MAX_TEST_FRAMES {
        return Err(format!(
            "frame count {frame_count} exceeds maximum supported {MAX_TEST_FRAMES}"
        )
        .into());
    }
    let pixel_count_u64 = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("width × height overflows u64")?;
    if pixel_count_u64 > MAX_TEST_PIXELS {
        return Err(format!(
            "frame area {pixel_count_u64} exceeds max supported {MAX_TEST_PIXELS} pixels"
        )
        .into());
    }
    let pixel_count =
        usize::try_from(pixel_count_u64).map_err(|_| "frame area does not fit into usize")?;
    let started = Instant::now();
    let tier_label = if write_mastering {
        "mastering"
    } else {
        "delivery"
    };
    eprintln!(
        "writing {}x{} {}-frame {tier_label} test file -> {}",
        width,
        height,
        frame_count,
        output.display()
    );
    let container_version_num = u16::from(container_version);
    let write_options = ContainerWriteOptions {
        container_version: container_version_num,
    };

    // Build a horizontal nit ramp mastering frame (Float64 linear light).
    let mastering_pixels: Vec<Pixel64> = (0..pixel_count)
        .map(|i| {
            let col = (i % width as usize) as f64;
            let nits = col / (width as f64 - 1.0).max(1.0) * 1000.0;
            Pixel64::new_unchecked(nits, nits, nits)
        })
        .collect();

    // Guard the .part file so we clean it up on any error path; only the
    // successful `commit()` at the end suppresses removal.
    let tmp_guard = TempFileGuard::new(part_path(output));
    let file = File::create(tmp_guard.path())
        .map_err(|e| format!("cannot create '{}': {e}", tmp_guard.path().display()))?;
    let mut buf = BufWriter::new(file);

    if write_mastering {
        // Write a mastering-tier file using the chosen lossless codec.
        let mastering_codec = resolve_mastering_codec(codec_arg)?;
        let static_meta = StaticMeta::default_mastering();

        let frames: Vec<MasteringFrame> = (0..frame_count)
            .map(|i| MasteringFrame {
                dynamic_meta: DynamicMeta::new(i as u64, 1000.0, 500.0),
                pixels: mastering_pixels.clone(),
            })
            .collect();

        write_mastering_file_with_options(
            &mut buf,
            width,
            height,
            &static_meta,
            &frames,
            mastering_codec,
            write_options,
        )
        .map_err(|e| format!("write error: {e}"))?;

        let codec_name = match codec_arg {
            MasteringCodecArg::Fpzip => "fpzip (pure Rust)",
            MasteringCodecArg::Zfp => "ZFP reversible",
        };
        println!("Written: {}", output.display());
        println!("  Tier              : mastering (Float64 linear light)");
        println!("  Mastering codec   : {codec_name}");
        println!(
            "  Dimensions        : {}×{}, {} frame(s)",
            width, height, frame_count
        );
        println!("  Container version : {container_version_num}");
        println!("  Pattern           : horizontal nit ramp, 0 – 1 000 nits");
        println!(
            "  First pixel       : R={:.4} nits  G={:.4} nits  B={:.4} nits",
            mastering_pixels[0].r, mastering_pixels[0].g, mastering_pixels[0].b
        );
        println!(
            "  Mid-column pixel  : R={:.1} nits",
            mastering_pixels[width as usize / 2].r
        );
    } else {
        // Transcode the mastering ramp to a delivery-tier AV1 file.
        let static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        let av1_cfg = Av1Config {
            speed,
            quantizer,
            lossless: quantizer == 0,
            // Keep test-vector generation reproducible across runs/machines.
            threads: 1,
            chroma: ChromaSampling420::Cs444,
        };

        let mut delivery_frames: Vec<DeliveryFrame> = Vec::with_capacity(frame_count as usize);
        for frame_idx in 0..frame_count {
            let result = transcode_frame(&mastering_pixels, frame_idx as u64, static_meta.clone())
                .map_err(|e| format!("transcode error on frame {frame_idx}: {e}"))?;

            delivery_frames.push(DeliveryFrame {
                dynamic_meta: result.dynamic_meta,
                pixels: result.pixels,
            });
        }

        write_delivery_file_with_options(
            &mut buf,
            width,
            height,
            &static_meta,
            &delivery_frames,
            &av1_cfg,
            write_options,
        )
        .map_err(|e| format!("write error: {e}"))?;

        let mid_col = width as usize / 2;
        let mid_pixel = &delivery_frames[0].pixels[mid_col];
        let mid_nits = pq_eotf_f32(mid_pixel.r) as f64 * PQ_MAX_NITS;

        println!("Written: {}", output.display());
        println!("  Tier              : delivery (AV1 12-bit 4:4:4, ST 2084 PQ, Rec. 2100)");
        println!("  AV1 quantizer     : {quantizer}, speed: {speed}");
        println!(
            "  Dimensions        : {}×{}, {} frame(s)",
            width, height, frame_count
        );
        println!("  Container version : {container_version_num}");
        println!("  Pattern           : horizontal nit ramp, 0 – 1 000 nits");
        println!(
            "  Mid-column pixel  : PQ R={:.6}  ≈ {mid_nits:.1} nits",
            mid_pixel.r
        );
    }

    let out_file = buf
        .into_inner()
        .map_err(|e| format!("flush error for '{}': {e}", tmp_guard.path().display()))?;
    out_file
        .sync_all()
        .map_err(|e| format!("sync error for '{}': {e}", tmp_guard.path().display()))?;
    // Take ownership back from the guard so we can rename without the
    // destructor racing to delete it; on error after this point the .part
    // file may persist, but at that point it has already been fsync'd.
    let tmp_path = tmp_guard.commit();
    fs::rename(&tmp_path, output).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            output.display()
        )
    })?;
    let elapsed_s = started.elapsed().as_secs_f64();
    let fps = if elapsed_s > 0.0 {
        frame_count as f64 / elapsed_s
    } else {
        0.0
    };
    eprintln!("done in {:.2}s ({:.1} fps average)", elapsed_s, fps);
    Ok(())
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn cmd_inspect(
    path: &std::path::Path,
    print_meta: bool,
    print_frames: bool,
    render_frame_time_ms: Option<f32>,
    render_target_max_nits: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path).map_err(|e| format!("cannot open '{}': {e}", path.display()))?;
    let mut stream =
        QdrvStreamReader::new(BufReader::new(file)).map_err(|e| format!("read error: {e}"))?;
    let header = stream.header().clone();
    let static_meta = stream.static_meta().clone();

    let tier_str = if header.tier == qdrv_io::container::TIER_DELIVERY {
        "delivery (AV1 12-bit 4:4:4, ST 2084 PQ, Rec. 2100)"
    } else {
        "mastering (Float64 linear light, Rec. 2100)"
    };

    let codec_unknown;
    let codec_str = match header.codec {
        0 => "raw (uncompressed)",
        1 => "AV1 delivery / fpzip or ZFP mastering",
        c => {
            codec_unknown = format!("unknown ({c})");
            &codec_unknown
        }
    };

    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│  QDRV file: {:<43} │", path.display());
    println!("└─────────────────────────────────────────────────────────┘");
    println!();
    println!("  Format version    : {}", header.version);
    println!("  Codec             : {codec_str}");
    println!("  Tier              : {tier_str}");
    println!("  Dimensions        : {}×{}", header.width, header.height);
    println!("  Frames            : {}", header.frame_count);
    println!("  Colour standard   : {}", static_meta.colour_standard);
    println!("  Transfer function : {}", static_meta.transfer_function);
    println!(
        "  Dynamic metadata  : {}",
        static_meta.dynamic_metadata_standard
    );
    println!(
        "  MaxCLL            : {:.1} nits",
        static_meta.content_light_level.max_cll_nits
    );
    println!(
        "  MaxFALL           : {:.1} nits",
        static_meta.content_light_level.max_fall_nits
    );

    if print_meta {
        println!();
        println!("Static metadata:");
        match qdrv_meta::to_json(&static_meta) {
            Ok(json) => println!("{json}"),
            Err(e) => println!("  (serialisation error: {e})"),
        }
    }

    if print_frames {
        println!();
        println!("Per-frame dynamic metadata:");
        let mut i = 0usize;
        while let Some(frame) = stream
            .next_frame()
            .map_err(|e| format!("read error: {e}"))?
        {
            match qdrv_meta::to_json(&frame.dynamic_meta) {
                Ok(json) => println!("Frame {i}:\n{json}"),
                Err(e) => println!("Frame {i}: (serialisation error: {e})"),
            }
            i += 1;
        }
    } else {
        println!();
        println!(
            "  {:>6}  {:>12}  {:>12}  {:>8}",
            "Frame", "Peak (nits)", "Avg (nits)", "Pixels"
        );
        println!(
            "  {:>6}  {:>12}  {:>12}  {:>8}",
            "-----", "----------", "---------", "------"
        );
        let mut first_delivery = None;
        let mut first_mastering = None;
        let mut temporal_state = TemporalStateManager::default();
        let mut i = 0usize;
        while let Some(frame) = stream
            .next_frame()
            .map_err(|e| format!("read error: {e}"))?
        {
            let dm = &frame.dynamic_meta;
            println!(
                "  {:>6}  {:>12.1}  {:>12.1}  {:>8}",
                i,
                dm.scene_peak_luminance_nits,
                dm.scene_average_luminance_nits,
                frame.pixels.len()
            );
            if let (Some(frame_time_ms), Some(delivery)) =
                (render_frame_time_ms, frame.pixels.as_delivery())
            {
                let mapped = tone_map_frame_with_policy_and_state(
                    delivery,
                    header.width,
                    header.height,
                    dm,
                    &TargetDisplay {
                        min_nits: 0.0005,
                        max_nits: render_target_max_nits.max(1.0),
                    },
                    &RenderPolicy {
                        frame_time_ms: Some(frame_time_ms),
                        ..RenderPolicy::default()
                    },
                    &mut temporal_state,
                );
                let avg_before = delivery
                    .iter()
                    .map(|p| (p.r + p.g + p.b) / 3.0)
                    .sum::<f32>()
                    / delivery.len().max(1) as f32;
                let avg_after = mapped.iter().map(|p| (p.r + p.g + p.b) / 3.0).sum::<f32>()
                    / mapped.len().max(1) as f32;
                println!(
                    "         render_sim frame_time={:.2}ms avg_pq={:.5}->{:.5}",
                    frame_time_ms, avg_before, avg_after
                );
            }
            if i == 0 {
                if let Some(pixels) = frame.pixels.as_delivery() {
                    if let Some(p) = pixels.first() {
                        first_delivery = Some(*p);
                    }
                } else if let Some(pixels) = frame.pixels.as_mastering()
                    && let Some(p) = pixels.first()
                {
                    first_mastering = Some(*p);
                }
            }
            i += 1;
        }

        // First-pixel summary.
        if let Some(p) = first_delivery {
            println!();
            println!(
                "  First pixel (delivery): R={:.6}  G={:.6}  B={:.6}",
                p.r, p.g, p.b
            );
        } else if let Some(p) = first_mastering {
            println!();
            println!(
                "  First pixel (mastering): R={:.4} nits  G={:.4} nits  B={:.4} nits",
                p.r, p.g, p.b
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// mux
// ---------------------------------------------------------------------------

/// Mux a delivery-tier `.qdrv32` file into a `.mp4` ISOBMFF container.
///
/// Re-encodes the QDRV file's decoded delivery-tier pixels through a stateful
/// [`TemporalEncoder`] (rav1e in temporal/GOP mode) and hands the resulting
/// AV1 packets to [`qdrv_mux::write_mp4`]. The output is a minimal but valid
/// ISOBMFF file with one AV1 video track and an HDR `colr nclx` box advertising
/// Rec. 2020 primaries, SMPTE ST 2084 transfer, and Rec. 2020 NCL matrix
/// coefficients.
///
/// # Errors
/// - Refuses any input whose container tier is not `TIER_DELIVERY` — the MP4
///   muxer is AV1-only; mastering-tier (`.qdrv64`) Float64 pixel data cannot
///   be carried in a standard ISOBMFF AV1 sample entry.
/// - Refuses `frame_rate <= 0`, non-finite, or out-of-range values; the
///   underlying muxer would reject the same and we report it earlier with a
///   more specific message.
/// - Propagates any rav1e / qdrv-mux / I/O error verbatim.
fn cmd_mux(
    input: &std::path::Path,
    output: &std::path::Path,
    frame_rate: f64,
    quantizer: usize,
    speed: u8,
    keyframe_interval: u32,
    format: MuxFormatArg,
) -> Result<(), Box<dyn std::error::Error>> {
    if !frame_rate.is_finite() || frame_rate <= 0.0 {
        return Err(
            format!("--frame-rate must be a positive, finite number (got {frame_rate})").into(),
        );
    }
    if keyframe_interval == 0 {
        return Err("--keyframe-interval must be >= 1".into());
    }

    let started = Instant::now();
    let in_file =
        File::open(input).map_err(|e| format!("cannot open '{}': {e}", input.display()))?;
    let mut stream =
        QdrvStreamReader::new(BufReader::new(in_file)).map_err(|e| format!("read error: {e}"))?;
    let header = stream.header().clone();

    if header.tier != TIER_DELIVERY {
        return Err(format!(
            "qdrv mux only accepts delivery-tier (.qdrv32) inputs; \
             '{}' is tier byte {} (expected {TIER_DELIVERY})",
            input.display(),
            header.tier
        )
        .into());
    }
    if header.frame_count == 0 {
        return Err(format!(
            "input '{}' declares zero frames; nothing to mux",
            input.display()
        )
        .into());
    }

    let av1_config = Av1Config {
        quantizer,
        speed,
        threads: 0,
        chroma: ChromaSampling420::Cs444,
        lossless: false,
    };
    let gop_config = GopConfig {
        keyframe_interval,
        max_b_frames: 0,
    };
    // Re-encode every decoded delivery frame to AV1. Mastering or raw-codec QDRV
    // inputs are rejected above, so `frame.pixels.as_delivery()` must succeed.
    let mut packets: Vec<EncodedPacket> = Vec::new();
    // Per-frame dynamic metadata, indexed by frame_index (the reader enforces
    // `frame_index == stream position`), so each AV1 packet can be tagged with
    // the metadata of the frame it encodes.
    let mut dynamic_metas: Vec<qdrv_meta::DynamicMeta> = Vec::new();
    let mut sent_frames: u64 = 0;

    // Prefer the temporal/GOP encoder (inter-frame prediction, smaller output).
    // rav1e's temporal path rejects some geometries that the still-picture path
    // accepts (e.g. a very small frame such as a 16x4 fixture), and QDRV can
    // legitimately produce such delivery files via `write-test`. When temporal
    // initialisation fails, fall back to encoding each frame as an independent
    // still picture so any valid `.qdrv32` can still be muxed.
    match TemporalEncoder::new(header.width, header.height, &av1_config, &gop_config) {
        Ok(mut encoder) => {
            while let Some(frame) = stream
                .next_frame()
                .map_err(|e| format!("read error at frame {sent_frames}: {e}"))?
            {
                let delivery = frame.pixels.as_delivery().ok_or_else(|| {
                    format!(
                        "frame {sent_frames}: delivery-tier file unexpectedly yielded \
                         non-delivery pixels (codec={})",
                        header.codec
                    )
                })?;
                encoder
                    .send_frame(delivery)
                    .map_err(|e| format!("frame {sent_frames}: send_frame failed: {e}"))?;
                dynamic_metas.push(frame.dynamic_meta);
                sent_frames += 1;
                let new_packets = encoder
                    .receive_packets()
                    .map_err(|e| format!("frame {sent_frames}: receive_packets failed: {e}"))?;
                packets.extend(new_packets);
            }
            packets.extend(
                encoder
                    .flush()
                    .map_err(|e| format!("temporal encoder flush failed: {e}"))?,
            );
        }
        Err(temporal_err) => {
            eprintln!(
                "note: temporal AV1 encoding unavailable for {}x{} ({temporal_err}); \
                 encoding independent still pictures (larger output)",
                header.width, header.height
            );
            while let Some(frame) = stream
                .next_frame()
                .map_err(|e| format!("read error at frame {sent_frames}: {e}"))?
            {
                let delivery = frame.pixels.as_delivery().ok_or_else(|| {
                    format!(
                        "frame {sent_frames}: delivery-tier file unexpectedly yielded \
                         non-delivery pixels (codec={})",
                        header.codec
                    )
                })?;
                let data = av1_encode(delivery, header.width, header.height, &av1_config).map_err(
                    |e| format!("frame {sent_frames}: still-picture encode failed: {e}"),
                )?;
                packets.push(EncodedPacket {
                    data,
                    frame_index: sent_frames,
                    is_keyframe: true,
                });
                dynamic_metas.push(frame.dynamic_meta);
                sent_frames += 1;
            }
        }
    }

    if packets.is_empty() {
        return Err(format!(
            "AV1 encoder produced no packets for '{}' ({} input frames)",
            input.display(),
            sent_frames
        )
        .into());
    }
    // rav1e may reorder packets across B-frames; sort by presentation index so
    // the MP4 sample table reflects display order. (We disable B-frames above,
    // so this is mostly defensive — but cheap.)
    packets.sort_by_key(|p| p.frame_index);

    // Embed each frame's dynamic metadata into its AV1 temporal unit as a QDRV
    // ITU-T T.35 metadata OBU, so every container/elementary target below
    // carries the metadata inside the bitstream rather than via a sidecar.
    let mut mux_frames: Vec<MuxFrame> = Vec::with_capacity(packets.len());
    for pkt in packets {
        let av1_data = match dynamic_metas.get(pkt.frame_index as usize) {
            Some(meta) => {
                let payload = qdrv_meta::binary::encode_dynamic_binary(meta).map_err(|e| {
                    format!(
                        "frame {}: dynamic metadata serialisation failed: {e}",
                        pkt.frame_index
                    )
                })?;
                qdrv_codec::embed_qdrv_metadata(&pkt.data, &payload).map_err(|e| {
                    format!("frame {}: metadata OBU embed failed: {e}", pkt.frame_index)
                })?
            }
            None => pkt.data,
        };
        mux_frames.push(MuxFrame {
            av1_data,
            is_keyframe: pkt.is_keyframe,
        });
    }

    let mp4_cfg = Mp4Config {
        frame_rate,
        width: header.width,
        height: header.height,
    };

    // DD-2: atomic write via `.part.<pid>` guarded by TempFileGuard so a
    // mid-write failure (codec error, disk full, hostile drop) cannot leave
    // a partial `.mp4` masquerading as a complete one. Matches the
    // `cmd_write_test` / `cmd_convert` pattern.
    let tmp_path = part_path(output);
    let tmp_guard = TempFileGuard::new(tmp_path);
    let out_file = File::create(tmp_guard.path())
        .map_err(|e| format!("cannot create '{}': {e}", tmp_guard.path().display()))?;
    let mut out_writer = BufWriter::new(out_file);
    let format_label = match format {
        MuxFormatArg::Mp4 => "mp4",
        MuxFormatArg::Fmp4 => "fmp4",
        MuxFormatArg::Cmaf => "cmaf",
        MuxFormatArg::Ivf => "ivf",
        MuxFormatArg::Obu => "obu",
    };
    match format {
        MuxFormatArg::Mp4 => write_mp4(&mut out_writer, &mp4_cfg, &mux_frames),
        MuxFormatArg::Fmp4 => write_fmp4(&mut out_writer, &mp4_cfg, &mux_frames),
        MuxFormatArg::Cmaf => write_cmaf(&mut out_writer, &mp4_cfg, &mux_frames),
        MuxFormatArg::Ivf => write_ivf(&mut out_writer, &mp4_cfg, &mux_frames),
        MuxFormatArg::Obu => write_obu_stream(&mut out_writer, &mux_frames),
    }
    .map_err(|e| format!("{format_label} mux failed: {e}"))?;
    let out_file = out_writer
        .into_inner()
        .map_err(|e| format!("mp4 writer flush failed: {e}"))?;
    out_file
        .sync_all()
        .map_err(|e| format!("mp4 fsync failed: {e}"))?;
    let tmp_path = tmp_guard.commit();
    fs::rename(&tmp_path, output).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            output.display()
        )
    })?;

    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    println!(
        "Wrote {} ({} frames, {} packets, {:.1} ms)",
        output.display(),
        sent_frames,
        mux_frames.len(),
        elapsed_ms
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// still
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AvifQdrvMetadata<'a> {
    qdrv_avif_metadata_version: u8,
    source_container_version: u16,
    source_tier: &'a str,
    frame_index: u64,
    static_meta: &'a StaticMeta,
    dynamic_meta: &'a DynamicMeta,
}

/// Export one QDRV frame as an AVIF still image.
fn cmd_still(
    input: &std::path::Path,
    output: &std::path::Path,
    frame_index: u32,
    quantizer: usize,
    speed: u8,
    deterministic: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let in_file =
        File::open(input).map_err(|e| format!("cannot open '{}': {e}", input.display()))?;
    let mut stream =
        QdrvStreamReader::new(BufReader::new(in_file)).map_err(|e| format!("read error: {e}"))?;
    let header = stream.header().clone();

    if header.frame_count == 0 {
        return Err(format!(
            "input '{}' declares zero frames; nothing to export",
            input.display()
        )
        .into());
    }
    if frame_index >= header.frame_count {
        return Err(format!(
            "--frame-index {frame_index} is out of range for '{}' ({} frame(s))",
            input.display(),
            header.frame_count
        )
        .into());
    }

    let mut selected = None;
    for current in 0..=frame_index {
        selected = stream
            .next_frame()
            .map_err(|e| format!("read error at frame {current}: {e}"))?;
    }
    let frame = selected.ok_or_else(|| {
        format!(
            "input '{}' ended before selected frame {frame_index}",
            input.display()
        )
    })?;

    let source_static_meta = stream.static_meta().clone();
    let source_tier = match header.tier {
        TIER_DELIVERY => "delivery",
        TIER_MASTERING => "mastering",
        other => {
            return Err(format!(
                "qdrv still only accepts delivery or mastering QDRV inputs; '{}' has tier byte {other}",
                input.display()
            )
            .into());
        }
    };

    let (pixels, static_meta, dynamic_meta) = if header.tier == TIER_DELIVERY {
        let pixels = frame
            .pixels
            .as_delivery()
            .ok_or_else(|| format!("frame {frame_index} is not delivery-tier"))?
            .to_vec();
        (pixels, source_static_meta, frame.dynamic_meta)
    } else {
        let mastering_pixels = frame
            .pixels
            .as_mastering()
            .ok_or_else(|| format!("frame {frame_index} is not mastering-tier"))?;
        let delivery_static_meta = delivery_static_meta_for_still(
            &source_static_meta,
            qdrv_meta::compatibility::METADATA_SCHEMA_V1,
        );
        let encode_options = EncodeOptions {
            deterministic,
            ..EncodeOptions::default()
        };
        let result = transcode_frame_with_options(
            mastering_pixels,
            u64::from(frame_index),
            delivery_static_meta,
            &encode_options,
        )
        .map_err(|e| format!("transcode error on frame {frame_index}: {e}"))?;
        (result.pixels, result.static_meta, result.dynamic_meta)
    };

    let av1_cfg = Av1Config {
        quantizer,
        speed,
        lossless: quantizer == 0,
        threads: if deterministic { 1 } else { 0 },
        chroma: ChromaSampling420::Cs444,
    };
    let av1_data = av1_encode(&pixels, header.width, header.height, &av1_cfg)
        .map_err(|e| format!("AV1 still encode failed for frame {frame_index}: {e}"))?;

    let metadata = AvifQdrvMetadata {
        qdrv_avif_metadata_version: 1,
        source_container_version: header.version,
        source_tier,
        frame_index: u64::from(frame_index),
        static_meta: &static_meta,
        dynamic_meta: &dynamic_meta,
    };
    let metadata_json = serde_json::to_vec_pretty(&metadata)
        .map_err(|e| format!("AVIF metadata serialisation failed: {e}"))?;

    let tmp_path = part_path(output);
    let tmp_guard = TempFileGuard::new(tmp_path);
    let out_file = File::create(tmp_guard.path())
        .map_err(|e| format!("cannot create '{}': {e}", tmp_guard.path().display()))?;
    let mut out_writer = BufWriter::new(out_file);
    write_avif(
        &mut out_writer,
        &AvifConfig::new(header.width, header.height),
        &av1_data,
        Some(&metadata_json),
    )
    .map_err(|e| format!("avif write failed: {e}"))?;
    let out_file = out_writer
        .into_inner()
        .map_err(|e| format!("avif writer flush failed: {e}"))?;
    out_file
        .sync_all()
        .map_err(|e| format!("avif fsync failed: {e}"))?;
    let tmp_path = tmp_guard.commit();
    fs::rename(&tmp_path, output).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            output.display()
        )
    })?;

    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    println!(
        "Wrote {} (source={}, frame={}, {}x{}, AV1 {} bytes, metadata {} bytes, {:.1} ms)",
        output.display(),
        source_tier,
        frame_index,
        header.width,
        header.height,
        av1_data.len(),
        metadata_json.len(),
        elapsed_ms
    );
    Ok(())
}

fn delivery_static_meta_for_still(source: &StaticMeta, metadata_schema_version: u16) -> StaticMeta {
    let mut static_meta = StaticMeta {
        qdrv_version: source.qdrv_version.clone(),
        metadata_schema_version,
        tier: Tier::Delivery,
        precision: Precision::Float32,
        colour_standard: source.colour_standard.clone(),
        colour_primaries: source.colour_primaries.clone(),
        transfer_function: "st2084_pq".to_string(),
        dynamic_metadata_standard: source.dynamic_metadata_standard.clone(),
        chroma_subsampling: source.chroma_subsampling,
        mastering_display: source.mastering_display.clone(),
        content_light_level: source.content_light_level,
        compatibility_tags: source.compatibility_tags.clone(),
    };
    if metadata_schema_version == qdrv_meta::compatibility::METADATA_SCHEMA_V2
        && !static_meta
            .compatibility_tags
            .iter()
            .any(|v| v == "open_dynamic_v2")
    {
        static_meta
            .compatibility_tags
            .push("open_dynamic_v2".to_string());
    }
    static_meta
}

// ---------------------------------------------------------------------------
// probe-stream
// ---------------------------------------------------------------------------

/// Reads embedded QDRV dynamic metadata back out of an exported AV1 elementary
/// stream (`.obu`) or IVF (`.ivf`) file and prints a per-frame summary.
fn cmd_probe_stream(input: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let data = fs::read(input).map_err(|e| format!("cannot read '{}': {e}", input.display()))?;
    let metas = probe_stream_metas(&data)?;
    if metas.is_empty() {
        println!("No embedded QDRV metadata found in {}", input.display());
        return Ok(());
    }
    println!(
        "{}: embedded QDRV dynamic metadata for {} frame(s)",
        input.display(),
        metas.len()
    );
    for meta in &metas {
        println!(
            "  frame {:>5}: scene_peak={:.1} nits  scene_avg={:.1} nits  open_v2={}",
            meta.frame_index,
            meta.scene_peak_luminance_nits,
            meta.scene_average_luminance_nits,
            meta.open_dynamic_v2.is_some()
        );
    }
    Ok(())
}

/// Detects the input format (ISOBMFF container, IVF, or raw OBU stream),
/// recovers the AV1 samples, extracts the QDRV metadata OBUs, and decodes each
/// into a [`qdrv_meta::DynamicMeta`]. Factored out of [`cmd_probe_stream`] so
/// extraction is unit-testable without stdout capture.
fn probe_stream_metas(data: &[u8]) -> Result<Vec<qdrv_meta::DynamicMeta>, String> {
    let av1 = if data.len() >= 4 && &data[0..4] == b"DKIF" {
        ivf_elementary_stream(data)?
    } else if data.len() >= 8 && &data[4..8] == b"ftyp" {
        qdrv_mux::extract_av1_samples(data).map_err(|e| format!("MP4 demux failed: {e}"))?
    } else {
        data.to_vec()
    };

    let payloads = qdrv_codec::extract_all_qdrv_metadata(&av1)
        .map_err(|e| format!("failed to parse AV1 elementary stream: {e}"))?;
    let mut metas = Vec::with_capacity(payloads.len());
    for (i, payload) in payloads.iter().enumerate() {
        let meta = qdrv_meta::binary::decode_dynamic_binary(payload)
            .map_err(|e| format!("frame {i}: embedded metadata failed to decode: {e}"))?;
        metas.push(meta);
    }
    Ok(metas)
}

/// Strips IVF framing, returning the concatenated AV1 temporal-unit bytes.
fn ivf_elementary_stream(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 32 || &data[0..4] != b"DKIF" {
        return Err("not a valid IVF file (missing DKIF header)".to_string());
    }
    let mut av1 = Vec::new();
    let mut offset = 32usize;
    while let Some(start) = offset.checked_add(12) {
        if start > data.len() {
            break;
        }
        let size = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        let end = start
            .checked_add(size)
            .ok_or_else(|| "IVF frame size overflow".to_string())?;
        if end > data.len() {
            return Err(format!(
                "truncated IVF frame at offset {offset}: declares {size} bytes, \
                 only {} remain",
                data.len() - start
            ));
        }
        av1.extend_from_slice(&data[start..end]);
        offset = end;
    }
    // A well-formed IVF stream consumes exactly to its end. Any 1..11 bytes left
    // over cannot form a complete 12-byte frame record, so the tail is truncated
    // or corrupt and must be reported rather than silently accepted.
    if offset != data.len() {
        return Err(format!(
            "{} trailing byte(s) after the last complete IVF frame: a frame \
             record needs a full 12-byte header",
            data.len() - offset
        ));
    }
    Ok(av1)
}

// ---------------------------------------------------------------------------
// convert
// ---------------------------------------------------------------------------

/// Converts a mastering-tier `.qdrv64` file to a delivery-tier `.qdrv32` file.
///
/// Reads every mastering frame (Float64 linear light), transcodes each
/// through the full QDRV encode pipeline (normalise to PQ, apply ST 2084
/// OETF, generate ST 2094-based dynamic metadata), then writes the
/// delivery container with AV1-compressed frames.
///
/// Optionally writes simultaneous sidecar outputs:
/// - `--sdr`: an 8-bit PPM file (Rec. 709 / sRGB) via `tone_map_to_sdr` (frame 0).
/// - `--hdr10`: a raw 10-bit RGB file (little-endian u16 triplets) via `to_hdr10_10bit` (frame 0).
///
/// Audit L-05 refactor: previously this function took 16 positional
/// arguments and tripped clippy's `too_many_arguments`. The arguments
/// are now grouped into the [`ConvertOptions`] struct below, which both
/// reduces caller noise and gives related fields (codec settings,
/// sidecar paths, v2 policy knobs) a shared home.
pub(crate) struct ConvertOptions<'a> {
    pub input: &'a std::path::Path,
    pub output: &'a std::path::Path,
    pub quantizer: usize,
    pub speed: u8,
    pub sdr_path: Option<&'a std::path::Path>,
    pub hdr10_path: Option<&'a std::path::Path>,
    pub deterministic: bool,
    pub creator_intent_lock: bool,
    pub metadata_v2: bool,
    pub ambient_lux: Option<f32>,
    pub display_model: Option<DisplayModelArg>,
    pub frame_time_ms: Option<f32>,
    pub fidelity_contract_path: Option<&'a std::path::Path>,
    pub interop_report_path: Option<&'a std::path::Path>,
    pub dv_sidecar_path: Option<&'a std::path::Path>,
    pub container_version: ContainerVersionArg,
}

fn cmd_convert(opts: ConvertOptions<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let ConvertOptions {
        input,
        output,
        quantizer,
        speed,
        sdr_path,
        hdr10_path,
        deterministic,
        creator_intent_lock,
        metadata_v2,
        ambient_lux,
        display_model,
        frame_time_ms,
        fidelity_contract_path,
        interop_report_path,
        dv_sidecar_path,
        container_version,
    } = opts;
    let file = File::open(input).map_err(|e| format!("cannot open '{}': {e}", input.display()))?;
    let mut stream =
        QdrvStreamReader::new(BufReader::new(file)).map_err(|e| format!("read error: {e}"))?;

    if stream.header().tier != TIER_MASTERING {
        return Err("input file is not a mastering-tier (.qdrv64) file".into());
    }
    if matches!(container_version, ContainerVersionArg::V1) && metadata_v2 {
        return Err(
            "--container-version v1 cannot be combined with --metadata-v2 (v1 requires metadata schema v1)"
                .into(),
        );
    }

    let w = stream.header().width;
    let h = stream.header().height;
    let source_static_meta = stream.static_meta().clone();
    // Preserve source static metadata fields where possible and only rewrite
    // the tier-specific fields required for delivery output.
    let mut static_meta = StaticMeta {
        qdrv_version: source_static_meta.qdrv_version.clone(),
        metadata_schema_version: if metadata_v2 {
            qdrv_meta::compatibility::METADATA_SCHEMA_V2
        } else {
            qdrv_meta::compatibility::METADATA_SCHEMA_V1
        },
        tier: Tier::Delivery,
        precision: Precision::Float32,
        colour_standard: source_static_meta.colour_standard.clone(),
        colour_primaries: source_static_meta.colour_primaries.clone(),
        transfer_function: "st2084_pq".to_string(),
        dynamic_metadata_standard: source_static_meta.dynamic_metadata_standard.clone(),
        chroma_subsampling: source_static_meta.chroma_subsampling,
        mastering_display: source_static_meta.mastering_display.clone(),
        content_light_level: source_static_meta.content_light_level,
        compatibility_tags: source_static_meta.compatibility_tags.clone(),
    };
    if metadata_v2
        && !static_meta
            .compatibility_tags
            .iter()
            .any(|v| v == "open_dynamic_v2")
    {
        static_meta
            .compatibility_tags
            .push("open_dynamic_v2".to_string());
    }
    let av1_cfg = Av1Config {
        speed,
        quantizer,
        lossless: quantizer == 0,
        threads: if deterministic { 1 } else { 0 },
        chroma: ChromaSampling420::Cs444,
    };

    let fidelity_contract = if let Some(path) = fidelity_contract_path {
        let data = fs::read_to_string(path)
            .map_err(|e| format!("cannot read fidelity contract '{}': {e}", path.display()))?;
        Some(
            qdrv_meta::from_json::<FidelityContract>(&data)
                .map_err(|e| format!("invalid fidelity contract '{}': {e}", path.display()))?,
        )
    } else {
        None
    };

    let mut delivery_frames: Vec<DeliveryFrame> = Vec::with_capacity(stream.frame_count() as usize);
    let mut hdr10_interop_report: Option<InteropLossReport> = None;
    let mut dv_interop_report: Option<InteropLossReport> = None;
    let mut dv_sidecars: Vec<interoperability::DolbyVisionCompatibleSidecar> = Vec::new();
    let mut i = 0usize;
    while let Some(frame) = stream
        .next_frame()
        .map_err(|e| format!("read error: {e}"))?
    {
        let mastering_pixels = frame
            .pixels
            .as_mastering()
            .ok_or_else(|| format!("frame {i} is not mastering-tier"))?;
        let open_dynamic_v2 = if metadata_v2 {
            Some(sample_open_dynamic_v2(
                ambient_lux,
                display_model,
                frame_time_ms,
            ))
        } else {
            None
        };
        let encode_options = EncodeOptions {
            deterministic,
            creator_intent_locked: creator_intent_lock,
            open_dynamic_v2,
            inverse_tone_mapping_hint: if metadata_v2 {
                Some(InverseToneMappingHint::default())
            } else {
                None
            },
        };

        let result = transcode_frame_with_options(
            mastering_pixels,
            i as u64,
            static_meta.clone(),
            &encode_options,
        )
        .map_err(|e| format!("transcode error on frame {i}: {e}"))?;

        if let Some(contract) = &fidelity_contract {
            // Audit MEDIUM (`AUDIT_REPORT_28-05-2026_2053.md`): read the
            // surrogate-acceptance env var once per convert run.
            let allow_vmaf_approximation = vmaf_hdr_approximation_allowed_from_env();
            let ref_pixels: Vec<qdrv_core::Pixel32> = mastering_pixels
                .iter()
                .map(|p| {
                    qdrv_core::Pixel32::new_unchecked(
                        pq_oetf_f32((p.r / PQ_MAX_NITS).clamp(0.0, 1.0) as f32),
                        pq_oetf_f32((p.g / PQ_MAX_NITS).clamp(0.0, 1.0) as f32),
                        pq_oetf_f32((p.b / PQ_MAX_NITS).clamp(0.0, 1.0) as f32),
                    )
                })
                .collect();
            let measurement = measure_fidelity(
                &ref_pixels,
                &result.pixels,
                w,
                h,
                i as u64,
                contract,
                allow_vmaf_approximation,
            )
            .map_err(|e| format!("fidelity measurement failed on frame {i}: {e}"))?;
            for note in &measurement.backend_notes {
                eprintln!("fidelity frame {i}: {note}");
            }
            let eval = contract.evaluate(&measurement.measured);
            if !eval.passed {
                return Err(format!(
                    "fidelity contract failed on frame {i}: {}",
                    eval.failures.join("; ")
                )
                .into());
            }
        }

        let frame_hdr10_report = interoperability::hdr10_loss_report(&result.dynamic_meta);
        hdr10_interop_report = Some(match hdr10_interop_report.take() {
            Some(existing) => merge_interop_reports(&existing, &frame_hdr10_report),
            None => frame_hdr10_report,
        });
        let (frame_sidecar, frame_dv_report) =
            interoperability::dolby_vision_compatible_sidecar(&result.dynamic_meta);
        dv_sidecars.push(frame_sidecar);
        dv_interop_report = Some(match dv_interop_report.take() {
            Some(existing) => merge_interop_reports(&existing, &frame_dv_report),
            None => frame_dv_report,
        });

        delivery_frames.push(DeliveryFrame {
            dynamic_meta: result.dynamic_meta,
            pixels: result.pixels,
        });
        i += 1;
    }

    // Cleanup guard for the .part file on any error path.
    let tmp_guard = TempFileGuard::new(part_path(output));
    let out_file = File::create(tmp_guard.path())
        .map_err(|e| format!("cannot create '{}': {e}", tmp_guard.path().display()))?;
    let mut wrt = BufWriter::new(out_file);
    let output_container_version = u16::from(container_version);
    write_delivery_file_with_options(
        &mut wrt,
        w,
        h,
        &static_meta,
        &delivery_frames,
        &av1_cfg,
        ContainerWriteOptions {
            container_version: output_container_version,
        },
    )
    .map_err(|e| format!("write error: {e}"))?;
    let out_file = wrt
        .into_inner()
        .map_err(|e| format!("flush error for '{}': {e}", tmp_guard.path().display()))?;
    out_file
        .sync_all()
        .map_err(|e| format!("sync error for '{}': {e}", tmp_guard.path().display()))?;
    let tmp_path = tmp_guard.commit();
    fs::rename(&tmp_path, output).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            output.display()
        )
    })?;

    println!("Converted: {} → {}", input.display(), output.display());
    println!("  Frames: {}, {}×{}", delivery_frames.len(), w, h);
    println!("  Container version: {output_container_version}");
    println!("  AV1 quantizer: {quantizer}, speed: {speed}");

    if let Some(sdr_out) = sdr_path {
        let first_frame = delivery_frames
            .first()
            .ok_or("converted stream has no frames; SDR sidecar requires at least one frame")?;
        let sdr_pixels = tone_map_to_sdr(&first_frame.pixels, &first_frame.dynamic_meta);
        write_ppm(sdr_out, w, h, &sdr_pixels)?;
        println!("  SDR fallback: {} (frame 0, PPM)", sdr_out.display());
    }

    if let Some(hdr10_out) = hdr10_path {
        let first_frame = delivery_frames
            .first()
            .ok_or("converted stream has no frames; HDR10 sidecar requires at least one frame")?;
        let hdr10_pixels = qdrv_encode::to_hdr10_10bit(&first_frame.pixels);
        write_hdr10_raw(hdr10_out, w, h, &hdr10_pixels)?;
        println!(
            "  HDR10 output: {} (frame 0, raw 10-bit RGB)",
            hdr10_out.display()
        );
    }

    if let Some(report_path) = interop_report_path {
        let report =
            hdr10_interop_report.ok_or("no frame available for interoperability report")?;
        let json = serde_json::to_string_pretty(&report)?;
        // DD-3: use atomic_write so a mid-write failure cannot leave a
        // partial report aliasing the previous run's complete one.
        atomic_write(report_path, json.as_bytes())?;
        println!("  Interop report: {}", report_path.display());
    }

    if let Some(sidecar_path) = dv_sidecar_path {
        if dv_sidecars.is_empty() {
            return Err("no frame available for DV-compatible sidecar".into());
        }
        let json = serde_json::to_string_pretty(&dv_sidecars)?;
        atomic_write(sidecar_path, json.as_bytes())?;
        let mut dv_report = dv_interop_report.ok_or("no frame available for DV loss report")?;
        dv_report.unsupported_features = union_string_lists(
            &dv_report.unsupported_features,
            &[
                "certified Dolby Vision bitstream generation requires external proprietary adapter"
                    .to_string(),
                "licensed vendor key material required for compliant packaging".to_string(),
            ],
        );
        let dv_report_path = sidecar_path.with_extension("loss-report.json");
        atomic_write(
            &dv_report_path,
            serde_json::to_string_pretty(&dv_report)?.as_bytes(),
        )?;
        println!(
            "  DV-compatible sidecar: {} (open representation); loss report: {}",
            sidecar_path.display(),
            dv_report_path.display()
        );
    }

    Ok(())
}

/// Writes HDR10-compatible 10-bit pixel data as raw little-endian u16 RGB triplets.
///
/// Each pixel is stored as three consecutive `u16` values (R, G, B) in
/// little-endian byte order, with values in the range `[0, 1023]`. The
/// file is row-major with no header — the caller must know the frame
/// dimensions to interpret it. This format can be fed directly into
/// any HDR10 muxing tool that accepts raw 10-bit PQ data.
fn write_hdr10_raw(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[[u16; 3]],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let expected_u64 = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("HDR10 width × height overflows u64")?;
    let expected = usize::try_from(expected_u64).map_err(|_| "HDR10 pixel count exceeds usize")?;
    if pixels.len() != expected {
        return Err(format!(
            "HDR10 pixel count mismatch: expected {expected}, got {}",
            pixels.len()
        )
        .into());
    }
    let mut f = BufWriter::new(File::create(path)?);
    for rgb in pixels {
        f.write_all(&rgb[0].to_le_bytes())?;
        f.write_all(&rgb[1].to_le_bytes())?;
        f.write_all(&rgb[2].to_le_bytes())?;
    }
    f.flush()?;
    Ok(())
}

/// Writes an array of 8-bit sRGB pixels as a binary PPM (P6) image file.
///
/// The PPM format is a simple uncompressed raster format: a plain-text
/// header (`P6\n<width> <height>\n255\n`) followed by raw RGB bytes in
/// row-major order. It is universally supported by image viewers and
/// requires no external libraries to produce.
fn write_ppm(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[[u8; 3]],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let expected_u64 = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("PPM width × height overflows u64")?;
    let expected = usize::try_from(expected_u64).map_err(|_| "PPM pixel count exceeds usize")?;
    if pixels.len() != expected {
        return Err(format!(
            "PPM pixel count mismatch: expected {expected}, got {}",
            pixels.len()
        )
        .into());
    }

    let mut f = BufWriter::new(File::create(path)?);
    write!(f, "P6\n{width} {height}\n255\n")?;
    for rgb in pixels {
        f.write_all(rgb)?;
    }
    f.flush()?;
    Ok(())
}

fn merge_interop_reports(base: &InteropLossReport, next: &InteropLossReport) -> InteropLossReport {
    InteropLossReport {
        target: base.target,
        dropped_fields: union_string_lists(&base.dropped_fields, &next.dropped_fields),
        approximated_fields: union_string_lists(
            &base.approximated_fields,
            &next.approximated_fields,
        ),
        unsupported_features: union_string_lists(
            &base.unsupported_features,
            &next.unsupported_features,
        ),
    }
}

fn union_string_lists(left: &[String], right: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut set = BTreeSet::new();
    for item in left {
        set.insert(item.clone());
    }
    for item in right {
        set.insert(item.clone());
    }
    set.into_iter().collect()
}

// ---------------------------------------------------------------------------
// hdr10plus
// ---------------------------------------------------------------------------

/// Exports per-frame HDR10+ profile metadata from a QDRV file.
///
/// Reads every frame's `DynamicMeta` and maps Float32 values to the
/// integer-valued fields defined by SMPTE ST 2094-40 and QDRV's open
/// profile extensions. The output is a machine-readable JSON object with:
///
/// - explicit `mode` (`basic`, `advanced`, `adaptive`, `gaming`)
/// - strict compatibility report (`not_certified` markers)
/// - per-frame entries for the selected mode
fn cmd_hdr10plus(
    input: &std::path::Path,
    output: &std::path::Path,
    mode: hdr10plus::Hdr10PlusProfileMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(input).map_err(|e| format!("cannot open '{}': {e}", input.display()))?;
    let mut stream =
        QdrvStreamReader::new(BufReader::new(file)).map_err(|e| format!("read error: {e}"))?;
    let mut metas = Vec::with_capacity(stream.frame_count() as usize);
    while let Some(frame) = stream
        .next_frame()
        .map_err(|e| format!("read error: {e}"))?
    {
        metas.push(frame.dynamic_meta);
    }
    let export = hdr10plus::build_profile_export(&metas, mode);
    atomic_write(output, serde_json::to_string_pretty(&export)?.as_bytes())?;

    let variant = match mode {
        hdr10plus::Hdr10PlusProfileMode::Basic => "HDR10+ basic (10-bit)",
        hdr10plus::Hdr10PlusProfileMode::Advanced => "HDR10+ advanced (16-bit)",
        hdr10plus::Hdr10PlusProfileMode::Adaptive => "HDR10+ adaptive-compatible",
        hdr10plus::Hdr10PlusProfileMode::Gaming => "HDR10+ gaming-compatible",
    };
    println!(
        "Exported {} frame(s) of {variant} metadata to {}",
        export.entries.len(),
        output.display()
    );
    let cert_status = match export.compatibility.certification_status {
        hdr10plus::Hdr10PlusCertificationStatus::Certified => "certified",
        hdr10plus::Hdr10PlusCertificationStatus::NotCertified => "not_certified",
    };
    println!(
        "  Certification status: {cert_status} (certified_output_generated={})",
        export.compatibility.certified_output_generated
    );
    Ok(())
}

/// Builds a v2 metadata sample payload populated with the **delivery-only**
/// adaptation fields (adaptation layer, ambient policy, gaming profile,
/// inverse tone-mapping hint). This is used by `qdrv convert` for the
/// delivery output and by `qdrv meta-dynamic-v2` for human inspection.
///
/// # Tier constraint
///
/// **This helper produces a payload that MUST NOT be embedded in a
/// mastering-tier (`.qdrv64`) file.** Per the `validate_compatibility` rule
/// in [`qdrv_meta::compatibility`], mastering-tier streams cannot carry
/// `adaptation_layer`, `ambient_policy`, `gaming_profile`, or
/// `inverse_tone_mapping_hint` — those are delivery-side adaptation
/// policies. Embedding this sample in a mastering file will be rejected
/// by the writer at `validate_compatibility` time. Callers building
/// mastering-tier metadata should construct an `OpenDynamicMetadataV2`
/// directly with only the creative-intent fields (`scene_constraints`,
/// `object_constraints`, `temporal`, `local_tone_map_grid`) populated.
///
/// Audit finding CC-1.
fn sample_open_dynamic_v2(
    ambient_lux: Option<f32>,
    display_model: Option<DisplayModelArg>,
    frame_time_ms: Option<f32>,
) -> OpenDynamicMetadataV2 {
    let ambient_policy = ambient_lux.map(|lux| AmbientAdaptivePolicy {
        lux_breakpoints: vec![0.0, lux.max(1.0), (lux * 4.0).max(8.0)],
        boost_multipliers: vec![1.0, 1.08, 1.18],
        max_delta_per_second: 0.6,
    });
    let adaptation_layer = Some(DisplayAdaptationLayer {
        source_mastering_peak_nits: 1000.0,
        abstract_display_peak_nits: 1000.0,
        display_model: display_model
            .map(DisplayModelClass::from)
            .unwrap_or(DisplayModelClass::Oled),
        highlight_rolloff_strength: 0.2,
        shadow_lift_strength: 0.08,
    });
    let frame_time_budget = frame_time_ms;
    let gaming_profile = frame_time_ms.map(|v| GamingProfile {
        frame_time_budget_ms: v.max(1.0),
        anti_pumping_strength: 0.8,
        max_gain_delta_per_frame: 0.05,
    });

    OpenDynamicMetadataV2 {
        scene_constraints: Vec::new(),
        object_constraints: Vec::new(),
        temporal: TemporalConstraint {
            max_global_gain_delta_per_frame: 0.08,
            anti_pumping_strength: 0.7,
            frame_time_budget_ms: frame_time_budget,
            integration_window_frames: None,
        },
        local_tone_map_grid: Some(LocalToneMapGrid::identity(4, 4)),
        adaptation_layer,
        ambient_policy,
        gaming_profile,
        inverse_tone_mapping_hint: Some(InverseToneMappingHint::default()),
        spherical_projection: None,
    }
}

fn cmd_export_interop(
    input: &std::path::Path,
    output_dir: &std::path::Path,
    dv_tool_cmd: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let summary = export_interop_bundle(input, output_dir, dv_tool_cmd)
        .map_err(|e| format!("interop export failed: {e}"))?;
    println!(
        "Interop bundle exported: frames={} output={}",
        summary.frame_count, summary.output_dir
    );
    println!("  HDR10 raw: {}", summary.hdr10_raw_path);
    println!("  HDR10+ JSON: {}", summary.hdr10plus_json_path);
    println!("  DV-compatible sidecar: {}", summary.dv_sidecar_path);
    println!("  Loss report: {}", summary.loss_report_path);
    println!("  DV adapter report: {}", summary.dv_adapter_report_path);
    if !summary.dv_adapter_status.invocation_succeeded {
        println!(
            "  DV proprietary adapter unavailable: {}",
            summary
                .dv_adapter_status
                .error
                .as_deref()
                .unwrap_or("missing proprietary capability")
        );
    }
    Ok(())
}

fn cmd_manifest_sign(
    input: &std::path::Path,
    output: &std::path::Path,
    key: &[u8],
    signer: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let payload = fs::read(input).map_err(|e| format!("cannot read '{}': {e}", input.display()))?;
    let manifest_payload = manifest::sign_manifest(&payload, signer, key)
        .map_err(|e| format!("manifest signing failed: {e}"))?;
    atomic_write(
        output,
        serde_json::to_string_pretty(&manifest_payload)?.as_bytes(),
    )?;
    println!(
        "Signed manifest written to {} (sha256={})",
        output.display(),
        manifest_payload.payload_hash_hex
    );
    Ok(())
}

fn cmd_manifest_verify(
    input: &std::path::Path,
    manifest_path: &std::path::Path,
    key: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let payload = fs::read(input).map_err(|e| format!("cannot read '{}': {e}", input.display()))?;
    let manifest_json = fs::read_to_string(manifest_path)
        .map_err(|e| format!("cannot read '{}': {e}", manifest_path.display()))?;
    let signed: qdrv_meta::SignedMetadataManifest = serde_json::from_str(&manifest_json)
        .map_err(|e| format!("invalid manifest JSON '{}': {e}", manifest_path.display()))?;
    manifest::verify_manifest(&payload, &signed, key)
        .map_err(|e| format!("manifest verification failed: {e}"))?;
    println!(
        "Manifest verification passed for {}",
        manifest_path.display()
    );
    Ok(())
}

fn cmd_conformance_generate_open(
    output_dir: &std::path::Path,
    corpus_name: &str,
    vectors: usize,
    width: u32,
    height: u32,
    key: &[u8],
    signer: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Audit MEDIUM (`AUDIT_REPORT_28-05-2026_2053.md`): the env var is
    // read once here so the library-side functions stay free of global
    // state. Unset (or anything other than "1"/"true") leaves
    // `vmaf_hdr_min` in any generated contract fail-closed when only the
    // deterministic surrogate is available.
    let allow_vmaf_approximation = vmaf_hdr_approximation_allowed_from_env();
    let config = OpenVectorsConfig {
        vector_count: vectors,
        width,
        height,
        allow_vmaf_approximation,
    };
    let manifest_path = generate_open_vectors(output_dir, corpus_name, &config, key, signer)
        .map_err(|e| format!("open vector generation failed: {e}"))?;
    println!(
        "Open conformance corpus generated: {}",
        manifest_path.display()
    );
    Ok(())
}

fn cmd_conformance_run(
    manifest_path: &std::path::Path,
    output_dir: &std::path::Path,
    key: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    // Same opt-in as in `cmd_conformance_generate_open`: unset env means
    // `vmaf_hdr_min` fails closed when only the surrogate is available.
    let allow_vmaf_approximation = vmaf_hdr_approximation_allowed_from_env();
    let summary = run_conformance(manifest_path, output_dir, key, allow_vmaf_approximation)
        .map_err(|e| format!("conformance run failed: {e}"))?;
    println!(
        "Conformance completed: {}/{} vectors passed",
        summary.passed_vectors, summary.total_vectors
    );
    println!(
        "  Summary: {}",
        output_dir.join("conformance-summary.json").display()
    );
    if !summary.all_passed {
        return Err("conformance failures detected (see summary JSON)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        io::BufWriter,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use clap::Parser;
    use qdrv_codec::Av1Config;
    use qdrv_io::writer::{DeliveryFrame, write_delivery_file};

    /// Audit LOW regression (`AUDIT_REPORT_2026-05-27_2339.md`): the
    /// resolver must fail-closed when no key is supplied and the
    /// public-default opt-in is absent. Previously this path printed a
    /// warning and proceeded.
    #[test]
    fn resolve_signing_key_or_default_fails_closed_without_opt_in() {
        let result =
            resolve_signing_key_or_default(None, None, CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY, false);
        let err = result.expect_err("default-key path must fail without opt-in");
        assert!(
            err.contains("--allow-public-default-key"),
            "error message must name the opt-in flag, got: {err}"
        );
    }

    /// Companion to the fail-closed test: when the opt-in flag is set,
    /// the resolver must succeed and return the built-in default bytes.
    #[test]
    fn resolve_signing_key_or_default_returns_default_with_opt_in() {
        let key =
            resolve_signing_key_or_default(None, None, CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY, true)
                .expect("opt-in path must succeed");
        assert_eq!(&*key, CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY);
    }

    /// Empty `--key` value (a common artefact of shell scripts trying to
    /// clear the variable) is still treated as unset for the
    /// fail-closed check, matching the P5-1 follow-up behaviour.
    #[test]
    fn resolve_signing_key_or_default_empty_key_is_unset() {
        let err = resolve_signing_key_or_default(
            Some(String::new()),
            None,
            CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY,
            false,
        )
        .expect_err("empty key without opt-in must fail-closed");
        assert!(
            err.contains("--allow-public-default-key"),
            "error must name the opt-in flag, got: {err}"
        );
    }

    /// An explicit key value bypasses the default-key opt-in entirely
    /// (the opt-in flag is meaningful only when no key was supplied).
    #[test]
    fn resolve_signing_key_or_default_uses_explicit_key() {
        let key = resolve_signing_key_or_default(
            Some("operator-key".to_string()),
            None,
            CONFORMANCE_OPEN_VECTORS_DEFAULT_KEY,
            false,
        )
        .expect("explicit key path must succeed without opt-in");
        assert_eq!(&*key, b"operator-key");
    }

    /// Audit LOW regression (`AUDIT_REPORT_28-05-2026_2053.md`): invalid
    /// `--nits=-1` previously printed an error to stderr but `cmd_pq`
    /// returned without signalling failure, so the process exited 0 and
    /// CI scripts treated rejection as success. The fix makes `cmd_pq`
    /// return `Result<(), _>` and routes errors through the existing
    /// `main` error path; the same non-zero exit fires for invalid `--pq`.
    #[test]
    fn cmd_pq_rejects_out_of_range_nits() {
        let result = cmd_pq(Some(-1.0), None);
        assert!(result.is_err(), "negative nits must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("luminance"),
            "error must cite luminance range, got: {msg}"
        );
    }

    /// Companion: invalid `--pq` value must also propagate as `Err`.
    #[test]
    fn cmd_pq_rejects_out_of_range_pq_signal() {
        let result = cmd_pq(None, Some(2.0));
        assert!(result.is_err(), "out-of-range PQ must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("PQ signal"),
            "error must cite PQ signal range, got: {msg}"
        );
    }

    /// `cmd_pq` with no arguments prints the reference table and returns
    /// `Ok(())`. This case must continue to succeed after the audit fix.
    #[test]
    fn cmd_pq_table_mode_succeeds() {
        cmd_pq(None, None).expect("table mode must succeed");
    }

    #[test]
    fn hdr10plus_cli_parses_mode_flag() {
        let cli = Cli::try_parse_from([
            "qdrv",
            "hdr10plus",
            "in.qdrv32",
            "out.json",
            "--mode",
            "adaptive",
        ])
        .expect("cli parse should succeed");
        match cli.command {
            Commands::Hdr10plus { mode, advanced, .. } => {
                assert!(matches!(mode, Hdr10PlusModeArg::Adaptive));
                assert!(!advanced);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn hdr10plus_cli_parses_legacy_advanced_flag() {
        let cli = Cli::try_parse_from(["qdrv", "hdr10plus", "in.qdrv32", "out.json", "--advanced"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Hdr10plus { advanced, .. } => assert!(advanced),
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn aces_export_cli_parses_defaults() {
        let cli = Cli::try_parse_from(["qdrv", "aces-export", "in.qdrv32", "aces-out"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::AcesExport {
                target,
                reference_white_nits,
                prefix,
                start_number,
                ..
            } => {
                assert_eq!(target, AcesExportTargetArg::Rec709100Nit);
                assert!((reference_white_nits - REFERENCE_WHITE_NITS).abs() < f64::EPSILON);
                assert_eq!(prefix, "frame");
                assert_eq!(start_number, 0);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn aces_export_cli_parses_all_targets() {
        for (flag, expected) in [
            ("aces2065-1", AcesExportTargetArg::Aces2065_1),
            ("rec709-100nit", AcesExportTargetArg::Rec709100Nit),
            ("rec2020-1000nit", AcesExportTargetArg::Rec20201000Nit),
            ("rec2020-4000nit", AcesExportTargetArg::Rec20204000Nit),
        ] {
            let cli = Cli::try_parse_from([
                "qdrv",
                "aces-export",
                "in.qdrv32",
                "aces-out",
                "--target",
                flag,
                "--reference-white-nits",
                "100",
                "--prefix",
                "shot",
                "--start-number",
                "42",
            ])
            .unwrap_or_else(|e| panic!("--target {flag} must parse: {e}"));
            match cli.command {
                Commands::AcesExport {
                    target,
                    reference_white_nits,
                    prefix,
                    start_number,
                    ..
                } => {
                    assert_eq!(target, expected, "--target {flag}");
                    assert!((reference_white_nits - 100.0).abs() < f64::EPSILON);
                    assert_eq!(prefix, "shot");
                    assert_eq!(start_number, 42);
                }
                _ => panic!("unexpected command parsed"),
            }
        }
    }

    #[test]
    fn still_cli_parses_defaults() {
        let cli = Cli::try_parse_from(["qdrv", "still", "in.qdrv32", "out.avif"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Still {
                frame_index,
                quantizer,
                speed,
                deterministic,
                ..
            } => {
                assert_eq!(frame_index, 0);
                assert_eq!(quantizer, 40);
                assert_eq!(speed, 6);
                assert!(!deterministic);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn still_cli_parses_options() {
        let cli = Cli::try_parse_from([
            "qdrv",
            "still",
            "in.qdrv64",
            "out.avif",
            "--frame-index",
            "3",
            "--quantizer",
            "12",
            "--speed",
            "4",
            "--deterministic",
        ])
        .expect("cli parse should succeed");
        match cli.command {
            Commands::Still {
                frame_index,
                quantizer,
                speed,
                deterministic,
                ..
            } => {
                assert_eq!(frame_index, 3);
                assert_eq!(quantizer, 12);
                assert_eq!(speed, 4);
                assert!(deterministic);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn still_command_writes_avif_from_delivery_fixture() {
        let root = make_temp_dir("still-delivery");
        let input = root.join("in.qdrv32");
        let output = root.join("out.avif");
        write_delivery_fixture_sized(&input, 2, 64, 64);

        cmd_still(&input, &output, 1, 40, 6, true).expect("still export should succeed");

        let bytes = fs::read(&output).expect("read avif");
        assert_eq!(&bytes[4..8], b"ftyp");
        assert_eq!(&bytes[8..12], b"avif");
        assert!(bytes.windows(4).any(|w| w == b"mif1"));
        assert!(bytes.windows(4).any(|w| w == b"avis"));
        let (meta_start, meta_end) = find_top_level_box(&bytes, b"meta").expect("meta box present");
        let meta = &bytes[meta_start..meta_end];
        assert!(meta.windows(4).any(|w| w == b"av01"));
        assert!(
            meta.windows(b"application/qdrv+json".len())
                .any(|w| w == b"application/qdrv+json")
        );
        assert!(find_top_level_box(&bytes, b"mdat").is_some());
        assert!(
            bytes
                .windows(b"\"source_tier\": \"delivery\"".len())
                .any(|w| w == b"\"source_tier\": \"delivery\"")
        );
        assert!(
            bytes
                .windows(b"\"frame_index\": 1".len())
                .any(|w| w == b"\"frame_index\": 1")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn still_command_writes_avif_from_mastering_fixture() {
        let root = make_temp_dir("still-mastering");
        let input = root.join("in.qdrv64");
        let output = root.join("out.avif");
        write_mastering_fixture_sized(&input, 1, 64, 64);

        cmd_still(&input, &output, 0, 40, 6, true).expect("still export should succeed");

        let bytes = fs::read(&output).expect("read avif");
        assert_eq!(&bytes[4..8], b"ftyp");
        assert_eq!(&bytes[8..12], b"avif");
        assert!(find_top_level_box(&bytes, b"meta").is_some());
        assert!(find_top_level_box(&bytes, b"mdat").is_some());
        assert!(
            bytes
                .windows(b"\"source_tier\": \"mastering\"".len())
                .any(|w| w == b"\"source_tier\": \"mastering\"")
        );
        assert!(
            bytes
                .windows(b"\"frame_index\": 0".len())
                .any(|w| w == b"\"frame_index\": 0")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn object_motion_cli_parses_endpoint_translate() {
        let cli = Cli::try_parse_from([
            "qdrv",
            "object-motion",
            "in.objectmeta.json",
            "out.objectmeta.json",
            "--region-id",
            "7",
            "--kind",
            "translate",
            "--frame-count",
            "5",
            "--to-x",
            "0.5",
            "--to-y",
            "0.25",
        ])
        .expect("cli parse should succeed");
        match cli.command {
            Commands::ObjectMotion {
                region_id,
                kind,
                frame_count,
                to_x,
                to_y,
                ..
            } => {
                assert_eq!(region_id, 7);
                assert_eq!(kind, ObjectMotionKindArg::Translate);
                assert_eq!(frame_count, Some(5));
                assert_eq!(to_x, Some(0.5));
                assert_eq!(to_y, Some(0.25));
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn object_motion_command_writes_valid_translate_descriptor() {
        let root = make_temp_dir("object-motion-translate");
        let input = root.join("in.objectmeta.json");
        let output = root.join("out.objectmeta.json");
        write_object_meta_fixture(&input);

        cmd_object_motion(ObjectMotionOptions {
            input: &input,
            output: &output,
            region_id: 7,
            kind: ObjectMotionKindArg::Translate,
            frame_count: Some(5),
            dx_per_frame: 0.0,
            dy_per_frame: 0.0,
            to_x: Some(0.5),
            to_y: Some(0.2),
            keyframes: &[],
            overwrite: false,
        })
        .expect("object-motion command should succeed");

        let json = fs::read_to_string(&output).expect("read object-motion output");
        let meta: qdrv_meta::ObjectMeta = qdrv_meta::from_json(&json).expect("parse output");
        meta.validate().expect("output metadata should validate");
        match meta.regions[0].motion.as_ref() {
            Some(qdrv_meta::RegionMotion::Translate {
                dx_per_frame,
                dy_per_frame,
                frame_count,
            }) => {
                assert!((*dx_per_frame - 0.1).abs() < f32::EPSILON);
                assert!((*dy_per_frame - 0.0).abs() < f32::EPSILON);
                assert_eq!(*frame_count, 5);
            }
            other => panic!("unexpected motion descriptor: {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn object_motion_command_writes_piecewise_descriptor() {
        let root = make_temp_dir("object-motion-piecewise");
        let input = root.join("in.objectmeta.json");
        let output = root.join("out.objectmeta.json");
        write_object_meta_fixture(&input);
        let keyframes = vec![
            "0:0:0".to_string(),
            "2:0.2:0.1".to_string(),
            "4:0.3:0.2".to_string(),
        ];

        cmd_object_motion(ObjectMotionOptions {
            input: &input,
            output: &output,
            region_id: 7,
            kind: ObjectMotionKindArg::PiecewiseLinear,
            frame_count: None,
            dx_per_frame: 0.0,
            dy_per_frame: 0.0,
            to_x: None,
            to_y: None,
            keyframes: &keyframes,
            overwrite: false,
        })
        .expect("piecewise object-motion command should succeed");

        let json = fs::read_to_string(&output).expect("read object-motion output");
        let meta: qdrv_meta::ObjectMeta = qdrv_meta::from_json(&json).expect("parse output");
        meta.validate().expect("output metadata should validate");
        assert!(meta.resolve_curve_at_frame(2, 0.35, 0.35).is_some());
        match meta.regions[0].motion.as_ref() {
            Some(qdrv_meta::RegionMotion::PiecewiseLinear { keyframes }) => {
                assert_eq!(keyframes.len(), 3);
                assert_eq!(keyframes[2].frame_delta, 4);
            }
            other => panic!("unexpected motion descriptor: {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mux_cli_parses_with_defaults() {
        let cli = Cli::try_parse_from(["qdrv", "mux", "in.qdrv32", "out.mp4"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Mux {
                frame_rate,
                quantizer,
                speed,
                keyframe_interval,
                format,
                ..
            } => {
                assert!((frame_rate - 24.0).abs() < f64::EPSILON);
                assert_eq!(quantizer, 40);
                assert_eq!(speed, 6);
                assert_eq!(keyframe_interval, 120);
                assert_eq!(format, MuxFormatArg::Mp4, "default format must be mp4");
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn mux_cli_parses_every_format_variant() {
        for (flag, expected) in [
            ("mp4", MuxFormatArg::Mp4),
            ("fmp4", MuxFormatArg::Fmp4),
            ("cmaf", MuxFormatArg::Cmaf),
            ("ivf", MuxFormatArg::Ivf),
            ("obu", MuxFormatArg::Obu),
        ] {
            let cli =
                Cli::try_parse_from(["qdrv", "mux", "in.qdrv32", "out.bin", "--format", flag])
                    .unwrap_or_else(|e| panic!("--format {flag} must parse: {e}"));
            match cli.command {
                Commands::Mux { format, .. } => {
                    assert_eq!(format, expected, "--format {flag}")
                }
                _ => panic!("unexpected command parsed"),
            }
        }
        // An unknown format is rejected by clap.
        assert!(
            Cli::try_parse_from(["qdrv", "mux", "in.qdrv32", "out.bin", "--format", "webm"])
                .is_err(),
            "unknown --format must be rejected"
        );
    }

    /// A synthetic AV1 temporal unit (temporal delimiter, sequence header,
    /// frame), each in low-overhead size-field form — enough to exercise the
    /// OBU walker and the metadata embed/extract path without invoking rav1e.
    fn synthetic_av1_temporal_unit() -> Vec<u8> {
        let mut tu = Vec::new();
        tu.extend_from_slice(&[0x12, 0]); // temporal delimiter (type 2)
        tu.extend_from_slice(&[0x0A, 3, 0xAA, 0xBB, 0xCC]); // sequence header (type 1)
        tu.extend_from_slice(&[0x32, 4, 0x11, 0x22, 0x33, 0x44]); // frame (type 6)
        tu
    }

    #[test]
    fn probe_stream_reads_embedded_metadata_from_obu() {
        let meta = qdrv_meta::DynamicMeta::new(7, 1234.0, 321.0);
        let payload = qdrv_meta::binary::encode_dynamic_binary(&meta).unwrap();
        let stream =
            qdrv_codec::embed_qdrv_metadata(&synthetic_av1_temporal_unit(), &payload).unwrap();

        let metas = probe_stream_metas(&stream).expect("probe must succeed");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].frame_index, 7);
        assert!((metas[0].scene_peak_luminance_nits - 1234.0).abs() < 0.1);
    }

    #[test]
    fn probe_stream_reads_metadata_through_ivf_framing() {
        let meta = qdrv_meta::DynamicMeta::new(3, 800.0, 150.0);
        let payload = qdrv_meta::binary::encode_dynamic_binary(&meta).unwrap();
        let tu = qdrv_codec::embed_qdrv_metadata(&synthetic_av1_temporal_unit(), &payload).unwrap();

        let mut ivf = vec![0u8; 32];
        ivf[0..4].copy_from_slice(b"DKIF");
        ivf.extend_from_slice(&(tu.len() as u32).to_le_bytes());
        ivf.extend_from_slice(&0u64.to_le_bytes()); // frame timestamp
        ivf.extend_from_slice(&tu);

        let metas = probe_stream_metas(&ivf).expect("ivf probe must succeed");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].frame_index, 3);
    }

    #[test]
    fn probe_stream_reads_metadata_from_mp4_family() {
        let meta = qdrv_meta::DynamicMeta::new(2, 1500.0, 400.0);
        let payload = qdrv_meta::binary::encode_dynamic_binary(&meta).unwrap();
        let tu = qdrv_codec::embed_qdrv_metadata(&synthetic_av1_temporal_unit(), &payload).unwrap();
        let frames = vec![qdrv_mux::MuxFrame {
            av1_data: tu,
            is_keyframe: true,
        }];
        let cfg = qdrv_mux::Mp4Config::new(24.0, 16, 16);

        let mut progressive = Vec::new();
        qdrv_mux::write_mp4(&mut progressive, &cfg, &frames).unwrap();
        let mut fragmented = Vec::new();
        qdrv_mux::write_fmp4(&mut fragmented, &cfg, &frames).unwrap();
        let mut cmaf = Vec::new();
        qdrv_mux::write_cmaf(&mut cmaf, &cfg, &frames).unwrap();

        for (label, bytes) in [
            ("mp4", &progressive),
            ("fmp4", &fragmented),
            ("cmaf", &cmaf),
        ] {
            let metas = probe_stream_metas(bytes)
                .unwrap_or_else(|e| panic!("{label} probe must succeed: {e}"));
            assert_eq!(metas.len(), 1, "{label}");
            assert_eq!(metas[0].frame_index, 2, "{label}");
        }
    }

    #[test]
    fn probe_stream_reports_empty_for_av1_without_metadata() {
        let metas = probe_stream_metas(&synthetic_av1_temporal_unit()).expect("probe must succeed");
        assert!(metas.is_empty());
    }

    /// L-01 regression (1008 audit): an IVF whose tail holds a partial,
    /// sub-12-byte frame record must be rejected, not silently accepted as a
    /// clean end of stream.
    #[test]
    fn ivf_parser_rejects_trailing_partial_frame_header() {
        let mut data = Vec::new();
        data.extend_from_slice(b"DKIF");
        data.extend_from_slice(&[0u8; 28]); // remainder of the 32-byte IVF header
        assert_eq!(data.len(), 32);
        data.extend_from_slice(&[0u8; 5]); // 5 trailing bytes: not a full record header
        let err = ivf_elementary_stream(&data).unwrap_err();
        assert!(err.contains("trailing"), "got {err}");
    }

    /// Rough-edge fix: a tiny 16x4 delivery file — a geometry rav1e's temporal
    /// path rejects but the still-picture path accepts — must still mux, via the
    /// still-picture fallback in `cmd_mux`.
    #[test]
    fn mux_succeeds_on_tiny_delivery_via_still_picture_fallback() {
        let root = make_temp_dir("mux-tiny-fallback");
        let input = root.join("tiny.qdrv32");
        let (w, h) = (16u32, 4u32);
        let pixels =
            vec![qdrv_core::pixel::Pixel32::new_unchecked(0.5, 0.5, 0.5); (w * h) as usize];
        let static_meta = qdrv_meta::StaticMeta::default_delivery(1000.0, 400.0);
        let av1_cfg = qdrv_codec::Av1Config {
            speed: 10,
            quantizer: 0,
            lossless: true,
            threads: 1,
            chroma: qdrv_codec::ChromaSampling420::Cs444,
        };
        let frame = qdrv_io::writer::DeliveryFrame {
            dynamic_meta: qdrv_meta::DynamicMeta::new(0, 1000.0, 400.0),
            pixels,
        };
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&input).unwrap());
            qdrv_io::writer::write_delivery_file(&mut f, w, h, &static_meta, &[frame], &av1_cfg)
                .unwrap();
            std::io::Write::flush(&mut f).unwrap();
        }
        let output = root.join("tiny.mp4");
        cmd_mux(&input, &output, 24.0, 40, 6, 120, MuxFormatArg::Mp4)
            .expect("tiny delivery file must mux via still-picture fallback");
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp", "output must be a valid MP4");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mux_rejects_mastering_tier_input() {
        let root = make_temp_dir("mux-mastering-rejected");
        // Write a *valid* mastering-tier .qdrv64 fixture (schema v1, no
        // delivery-only adaptation fields — the compatibility rule in
        // `qdrv-meta::compatibility` rejects mastering+adaptation_layer /
        // gaming_profile / inverse_tone_mapping_hint at the writer, and
        // here we want the *tier* gate inside `cmd_mux` to be the
        // rejection path under test, not the writer-side compatibility
        // gate).
        let input = root.join("in.qdrv64");
        let mastering_pixels = vec![
            qdrv_core::pixel::Pixel64 {
                r: 100.0,
                g: 100.0,
                b: 100.0,
            };
            4
        ];
        let mut static_meta = StaticMeta::default_mastering();
        static_meta.metadata_schema_version = qdrv_meta::compatibility::METADATA_SCHEMA_V1;
        let mut dynamic = DynamicMeta::new(0, 1000.0, 200.0);
        dynamic.metadata_schema_version = static_meta.metadata_schema_version;
        let frames = vec![MasteringFrame {
            dynamic_meta: dynamic,
            pixels: mastering_pixels,
        }];
        let mut writer = BufWriter::new(File::create(&input).expect("create mastering fixture"));
        qdrv_io::writer::write_mastering_file(
            &mut writer,
            2,
            2,
            &static_meta,
            &frames,
            MasteringCodec::Fpzip,
        )
        .expect("write mastering fixture");
        use std::io::Write;
        writer.flush().expect("flush mastering fixture");

        let output = root.join("out.mp4");
        let err = cmd_mux(&input, &output, 24.0, 40, 6, 120, MuxFormatArg::Mp4)
            .expect_err("mux must reject mastering tier");
        let msg = format!("{err}");
        assert!(
            msg.contains("delivery-tier"),
            "expected tier-rejection diagnostic, got {msg:?}"
        );
        assert!(!output.exists(), "mux must not create output on rejection");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mux_produces_valid_mp4_from_delivery_fixture() {
        let root = make_temp_dir("mux-mp4-roundtrip");
        let input = root.join("in.qdrv32");
        // 64x64 is well above rav1e's minimum-block constraints.
        write_delivery_fixture_sized(&input, 2, 64, 64);

        let output = root.join("out.mp4");
        cmd_mux(&input, &output, 24.0, 40, 6, 120, MuxFormatArg::Mp4)
            .expect("mux should succeed on delivery input");

        let bytes = fs::read(&output).expect("read produced mp4");
        // ISOBMFF `ftyp` box: size (4) + 'ftyp' (4) + major_brand 'isom' (4).
        assert!(bytes.len() > 16, "mp4 must be larger than ftyp header");
        assert_eq!(&bytes[4..8], b"ftyp", "first box must be ftyp");
        assert_eq!(&bytes[8..12], b"isom", "major brand must be isom");
        // Spot-check that an `mdat` payload box appears somewhere after `moov`.
        assert!(
            bytes.windows(4).any(|w| w == b"moov"),
            "mp4 must contain a moov box"
        );
        assert!(
            bytes.windows(4).any(|w| w == b"mdat"),
            "mp4 must contain an mdat box"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hdr10plus_export_supports_all_modes_with_profile_metadata() {
        let root = make_temp_dir("hdr10plus-modes");
        let input = root.join("in.qdrv32");
        write_delivery_fixture(&input, 2);

        for (mode, mode_name) in [
            (hdr10plus::Hdr10PlusProfileMode::Basic, "basic"),
            (hdr10plus::Hdr10PlusProfileMode::Advanced, "advanced"),
            (hdr10plus::Hdr10PlusProfileMode::Adaptive, "adaptive"),
            (hdr10plus::Hdr10PlusProfileMode::Gaming, "gaming"),
        ] {
            let output = root.join(format!("{mode_name}.json"));
            cmd_hdr10plus(&input, &output, mode).expect("hdr10plus export should succeed");
            let json = fs::read_to_string(&output).expect("read export");
            let value: serde_json::Value = serde_json::from_str(&json).expect("parse export");
            assert_eq!(value["mode"].as_str(), Some(mode_name));
            assert_eq!(
                value["compatibility"]["certification_status"].as_str(),
                Some("not_certified")
            );
            assert_eq!(
                value["compatibility"]["certified_output_generated"].as_bool(),
                Some(false)
            );
            assert_eq!(value["entries"].as_array().map(Vec::len), Some(2));
            for entry in value["entries"].as_array().expect("entries array") {
                assert_eq!(entry["profile"].as_str(), Some(mode_name));
            }
        }

        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir(label: &str) -> PathBuf {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        root.push(format!("qdrv-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("create temp dir");
        root
    }

    fn write_delivery_fixture(path: &Path, frame_count: u64) {
        write_delivery_fixture_sized(path, frame_count, 2, 2);
    }

    fn write_delivery_fixture_sized(path: &Path, frame_count: u64, width: u32, height: u32) {
        let pixels = vec![
            qdrv_core::pixel::Pixel32::new_unchecked(0.1, 0.2, 0.3);
            (width * height) as usize
        ];
        let mut static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        static_meta.metadata_schema_version = qdrv_meta::compatibility::METADATA_SCHEMA_V2;
        let mut frames = Vec::with_capacity(frame_count as usize);
        for idx in 0..frame_count {
            let mut dynamic = DynamicMeta::new(idx, 1000.0 + idx as f32 * 25.0, 200.0);
            dynamic.metadata_schema_version = static_meta.metadata_schema_version;
            dynamic.open_dynamic_v2 = Some(sample_open_dynamic_v2(
                Some(120.0),
                Some(DisplayModelArg::Oled),
                Some(8.3),
            ));
            frames.push(DeliveryFrame {
                dynamic_meta: dynamic,
                pixels: pixels.clone(),
            });
        }

        let mut writer = BufWriter::new(File::create(path).expect("create fixture"));
        write_delivery_file(
            &mut writer,
            width,
            height,
            &static_meta,
            &frames,
            &Av1Config {
                threads: 1,
                ..Av1Config::default()
            },
        )
        .expect("write fixture");
        use std::io::Write;
        writer.flush().expect("flush fixture");
    }

    fn write_mastering_fixture_sized(path: &Path, frame_count: u64, width: u32, height: u32) {
        let pixels = vec![
            qdrv_core::pixel::Pixel64 {
                r: 100.0,
                g: 120.0,
                b: 140.0,
            };
            (width * height) as usize
        ];
        let mut static_meta = StaticMeta::default_mastering();
        static_meta.metadata_schema_version = qdrv_meta::compatibility::METADATA_SCHEMA_V1;
        let mut frames = Vec::with_capacity(frame_count as usize);
        for idx in 0..frame_count {
            let mut dynamic = DynamicMeta::new(idx, 1000.0 + idx as f32 * 25.0, 200.0);
            dynamic.metadata_schema_version = static_meta.metadata_schema_version;
            frames.push(MasteringFrame {
                dynamic_meta: dynamic,
                pixels: pixels.clone(),
            });
        }

        let mut writer = BufWriter::new(File::create(path).expect("create fixture"));
        qdrv_io::writer::write_mastering_file(
            &mut writer,
            width,
            height,
            &static_meta,
            &frames,
            MasteringCodec::Fpzip,
        )
        .expect("write fixture");
        use std::io::Write;
        writer.flush().expect("flush fixture");
    }

    fn find_top_level_box(data: &[u8], typ: &[u8; 4]) -> Option<(usize, usize)> {
        let mut pos = 0usize;
        while pos.checked_add(8)? <= data.len() {
            let header_end = pos.checked_add(8)?;
            let size =
                usize::try_from(u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?)).ok()?;
            if size < 8 {
                return None;
            }
            let end = pos.checked_add(size)?;
            if end > data.len() {
                return None;
            }
            if &data[pos + 4..header_end] == typ {
                return Some((pos, end));
            }
            pos = end;
        }
        None
    }

    fn write_object_meta_fixture(path: &Path) {
        let meta = qdrv_meta::ObjectMeta {
            frame_index: 0,
            regions: vec![qdrv_meta::ObjectRegion {
                id: 7,
                bounding_box: qdrv_meta::BoundingBox {
                    x: 0.1,
                    y: 0.2,
                    width: 0.2,
                    height: 0.2,
                },
                tone_map_curve: qdrv_meta::ToneMapCurve::linear(),
                priority: 9,
                motion: None,
            }],
            spherical_regions: Vec::new(),
        };
        fs::write(
            path,
            qdrv_meta::to_json(&meta).expect("serialise object meta"),
        )
        .expect("write object meta fixture");
    }
}
