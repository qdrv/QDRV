// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Interoperability bundle export for the `qdrv` CLI.
//!
//! Builds a practical hand-off bundle from QDRV input: HDR10 raw payload,
//! HDR10+ JSON, an open Dolby Vision-compatible sidecar, and explicit loss
//! reporting that documents what cannot be represented in each target.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::Path,
    process::Command,
};

use qdrv_core::pixel::Pixel32;
use qdrv_encode::{EncodeOptions, to_hdr10_10bit, transcode_frame_with_options};
use qdrv_io::reader::QdrvStreamReader;
use qdrv_meta::{
    DynamicMeta, InteropLossReport, InteropTarget, StaticMeta, hdr10plus, interoperability,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DvAdapterStatus {
    pub(crate) configured_command: Option<String>,
    pub(crate) command_detected: bool,
    pub(crate) invocation_attempted: bool,
    pub(crate) invocation_succeeded: bool,
    pub(crate) rpu_output_path: Option<String>,
    pub(crate) missing_capabilities: Vec<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InteropExportSummary {
    pub(crate) schema_version: u16,
    pub(crate) input_path: String,
    pub(crate) output_dir: String,
    pub(crate) frame_count: usize,
    pub(crate) hdr10_raw_path: String,
    pub(crate) hdr10plus_json_path: String,
    pub(crate) dv_sidecar_path: String,
    pub(crate) loss_report_path: String,
    pub(crate) dv_adapter_report_path: String,
    pub(crate) dv_adapter_status: DvAdapterStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CombinedLossReport {
    schema_version: u16,
    frame_count: usize,
    hdr10: InteropLossReport,
    hdr10plus: InteropLossReport,
    hdr10plus_compatibility: hdr10plus::Hdr10PlusCompatibilityReport,
    dolby_vision_compatible: InteropLossReport,
    dv_adapter_status: DvAdapterStatus,
}

pub(crate) fn export_interop_bundle(
    input: &Path,
    output_dir: &Path,
    dv_tool_cmd: Option<&str>,
) -> Result<InteropExportSummary, String> {
    fs::create_dir_all(output_dir).map_err(|e| {
        format!(
            "cannot create output directory '{}': {e}",
            output_dir.display()
        )
    })?;

    let (frames, width, height) = read_or_transcode_to_delivery_frames(input)?;
    if frames.is_empty() {
        return Err("input has no frames".to_string());
    }

    let hdr10_raw_path = output_dir.join("interop.hdr10.rgb10le.raw");
    let hdr10plus_path = output_dir.join("interop.hdr10plus.json");
    let dv_sidecar_path = output_dir.join("interop.dv-compatible.json");
    let loss_report_path = output_dir.join("interop.loss-report.json");
    let dv_adapter_report_path = output_dir.join("interop.dv-adapter-report.json");
    let dv_rpu_output_path = output_dir.join("interop.dv.rpu.bin");

    write_hdr10_raw_sequence(&hdr10_raw_path, width, height, &frames)?;

    let hdr10plus_metas = frames.iter().map(|f| f.dynamic.clone()).collect::<Vec<_>>();
    let hdr10plus_export =
        hdr10plus::build_profile_export(&hdr10plus_metas, hdr10plus::Hdr10PlusProfileMode::Basic);
    fs::write(
        &hdr10plus_path,
        serde_json::to_string_pretty(&hdr10plus_export)
            .map_err(|e| format!("failed serializing HDR10+ sidecar: {e}"))?,
    )
    .map_err(|e| format!("failed writing '{}': {e}", hdr10plus_path.display()))?;

    let mut dv_sidecars = Vec::with_capacity(frames.len());
    let mut hdr10_loss = interoperability::hdr10_loss_report(&frames[0].dynamic);
    let mut dv_loss = interoperability::dolby_vision_compatible_sidecar(&frames[0].dynamic).1;
    for frame in &frames {
        let (sidecar, per_frame_dv_loss) =
            interoperability::dolby_vision_compatible_sidecar(&frame.dynamic);
        dv_sidecars.push(sidecar);
        hdr10_loss = merge_reports(
            &hdr10_loss,
            &interoperability::hdr10_loss_report(&frame.dynamic),
        );
        dv_loss = merge_reports(&dv_loss, &per_frame_dv_loss);
    }

    // GG-3: We previously wrote the sidecar twice — once as adapter input,
    // once with the adapter-derived status patched in. A failure between
    // the two writes left an inconsistent sidecar on disk reflecting the
    // pre-adapter state. The adapter only needs sidecar bytes to *exist*
    // for the duration of its invocation, so we stage the bytes through a
    // sibling `.adapter-input.json` file that we delete after the adapter
    // returns; the persistent on-disk sidecar at `dv_sidecar_path` is
    // written exactly once with the final, status-aware payload.
    let initial_sidecar_bytes = serde_json::to_vec_pretty(&dv_sidecars)
        .map_err(|e| format!("failed serializing DV-compatible sidecars: {e}"))?;
    let adapter_input_path = dv_sidecar_path.with_extension("adapter-input.json");
    fs::write(&adapter_input_path, &initial_sidecar_bytes).map_err(|e| {
        format!(
            "failed staging adapter input '{}': {e}",
            adapter_input_path.display()
        )
    })?;
    // RAII cleanup for the staged adapter input regardless of which exit
    // path the adapter takes; mirrors the `TempFileGuard` pattern from
    // `qdrv-tool/src/main.rs`. Defined inline to keep the interop_export
    // module self-contained.
    struct StagedSidecarGuard {
        path: Option<std::path::PathBuf>,
    }
    impl Drop for StagedSidecarGuard {
        fn drop(&mut self) {
            if let Some(path) = self.path.take() {
                let _ = fs::remove_file(&path);
            }
        }
    }
    let _adapter_input_guard = StagedSidecarGuard {
        path: Some(adapter_input_path.clone()),
    };

    let dv_adapter_status = run_dv_adapter(
        dv_tool_cmd,
        &adapter_input_path,
        &dv_rpu_output_path,
        &dv_adapter_report_path,
    );
    if dv_adapter_status.invocation_succeeded {
        // GG-5: remove the same strings that
        // `dolby_vision_compatible_sidecar` injected, via the exported
        // constants — so changing the wording in one place updates both
        // sites in lock-step.
        let mut unsupported = BTreeSet::new();
        unsupported.extend(dv_loss.unsupported_features.iter().cloned());
        unsupported.remove(interoperability::DV_LOSS_UNSUPPORTED_CERTIFIED_BITSTREAM);
        unsupported.remove(interoperability::DV_LOSS_UNSUPPORTED_VENDOR_KEYS);
        dv_loss.unsupported_features = unsupported.into_iter().collect();
    } else {
        dv_loss.unsupported_features = union_lists(
            &dv_loss.unsupported_features,
            &dv_adapter_status.missing_capabilities,
        );
    }

    apply_dv_adapter_status_to_sidecars(&mut dv_sidecars, &dv_adapter_status);

    // Single, final write of the persistent sidecar with the adapter-aware
    // status applied. The previous two-write scheme is gone.
    fs::write(
        &dv_sidecar_path,
        serde_json::to_string_pretty(&dv_sidecars)
            .map_err(|e| format!("failed serializing DV-compatible sidecars: {e}"))?,
    )
    .map_err(|e| format!("failed writing '{}': {e}", dv_sidecar_path.display()))?;

    fs::write(
        &dv_adapter_report_path,
        serde_json::to_string_pretty(&dv_adapter_status)
            .map_err(|e| format!("failed serializing DV adapter status: {e}"))?,
    )
    .map_err(|e| format!("failed writing '{}': {e}", dv_adapter_report_path.display()))?;

    let hdr10plus_compatibility =
        hdr10plus::compatibility_report(hdr10plus::Hdr10PlusProfileMode::Basic);
    let mut hdr10plus_unsupported = vec![
        "proprietary vendor extension blocks".to_string(),
        "certification_status=not_certified".to_string(),
    ];
    hdr10plus_unsupported.extend(
        hdr10plus_compatibility
            .missing_capabilities
            .iter()
            .map(|v| format!("missing certification capability: {v}")),
    );
    let hdr10plus_loss = InteropLossReport {
        target: InteropTarget::Hdr10Plus,
        dropped_fields: vec![
            "open_dynamic_v2.local_tone_map_grid".to_string(),
            "open_dynamic_v2.object_constraints".to_string(),
        ],
        approximated_fields: vec![
            "tone_map_curve anchors quantized to 10/16-bit integers".to_string(),
            "scene luminance compressed into HDR10+ maxRGB/statistical slots".to_string(),
        ],
        unsupported_features: hdr10plus_unsupported,
    };

    let combined = CombinedLossReport {
        schema_version: 1,
        frame_count: frames.len(),
        hdr10: hdr10_loss,
        hdr10plus: hdr10plus_loss,
        hdr10plus_compatibility,
        dolby_vision_compatible: dv_loss,
        dv_adapter_status: dv_adapter_status.clone(),
    };
    fs::write(
        &loss_report_path,
        serde_json::to_string_pretty(&combined)
            .map_err(|e| format!("failed serializing loss report: {e}"))?,
    )
    .map_err(|e| format!("failed writing '{}': {e}", loss_report_path.display()))?;

    Ok(InteropExportSummary {
        schema_version: 1,
        input_path: input.display().to_string(),
        output_dir: output_dir.display().to_string(),
        frame_count: frames.len(),
        hdr10_raw_path: hdr10_raw_path.display().to_string(),
        hdr10plus_json_path: hdr10plus_path.display().to_string(),
        dv_sidecar_path: dv_sidecar_path.display().to_string(),
        loss_report_path: loss_report_path.display().to_string(),
        dv_adapter_report_path: dv_adapter_report_path.display().to_string(),
        dv_adapter_status,
    })
}

#[derive(Debug, Clone)]
struct DeliveryFrameLike {
    dynamic: DynamicMeta,
    pixels: Vec<Pixel32>,
}

fn read_or_transcode_to_delivery_frames(
    input: &Path,
) -> Result<(Vec<DeliveryFrameLike>, u32, u32), String> {
    let file =
        File::open(input).map_err(|e| format!("cannot open input '{}': {e}", input.display()))?;
    let mut stream = QdrvStreamReader::new(BufReader::new(file))
        .map_err(|e| format!("cannot parse input '{}': {e}", input.display()))?;
    let width = stream.header().width;
    let height = stream.header().height;

    let mut out = Vec::with_capacity(stream.frame_count() as usize);
    let mut frame_index = 0_u64;
    while let Some(frame) = stream
        .next_frame()
        .map_err(|e| format!("cannot decode frame {frame_index}: {e}"))?
    {
        if let Some(pixels) = frame.pixels.as_delivery() {
            out.push(DeliveryFrameLike {
                dynamic: frame.dynamic_meta,
                pixels: pixels.to_vec(),
            });
        } else if let Some(mastering) = frame.pixels.as_mastering() {
            let static_meta = StaticMeta::default_delivery(1000.0, 300.0);
            let encoded = transcode_frame_with_options(
                mastering,
                frame_index,
                static_meta,
                &EncodeOptions {
                    deterministic: true,
                    ..EncodeOptions::default()
                },
            )
            .map_err(|e| format!("cannot transcode mastering frame {frame_index}: {e}"))?;
            out.push(DeliveryFrameLike {
                dynamic: encoded.dynamic_meta,
                pixels: encoded.pixels,
            });
        }
        frame_index += 1;
    }

    Ok((out, width, height))
}

fn write_hdr10_raw_sequence(
    path: &Path,
    width: u32,
    height: u32,
    frames: &[DeliveryFrameLike],
) -> Result<(), String> {
    let expected_pixels = usize::try_from(
        u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| "HDR10 width × height overflow".to_string())?,
    )
    .map_err(|_| "HDR10 width × height exceeds usize".to_string())?;
    let mut writer = BufWriter::new(
        File::create(path).map_err(|e| format!("cannot create '{}': {e}", path.display()))?,
    );
    for (idx, frame) in frames.iter().enumerate() {
        if frame.pixels.len() != expected_pixels {
            return Err(format!(
                "frame {idx} pixel count mismatch: expected {expected_pixels}, got {}",
                frame.pixels.len()
            ));
        }
        let quantized = to_hdr10_10bit(&frame.pixels);
        for rgb in &quantized {
            writer
                .write_all(&rgb[0].to_le_bytes())
                .map_err(|e| format!("failed writing HDR10 frame {idx}: {e}"))?;
            writer
                .write_all(&rgb[1].to_le_bytes())
                .map_err(|e| format!("failed writing HDR10 frame {idx}: {e}"))?;
            writer
                .write_all(&rgb[2].to_le_bytes())
                .map_err(|e| format!("failed writing HDR10 frame {idx}: {e}"))?;
        }
    }
    writer
        .flush()
        .map_err(|e| format!("failed flushing HDR10 output '{}': {e}", path.display()))?;
    Ok(())
}

fn dv_missing_capabilities() -> Vec<String> {
    interoperability::DV_REQUIRED_PROPRIETARY_CAPABILITIES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn apply_dv_adapter_status_to_sidecars(
    sidecars: &mut [interoperability::DolbyVisionCompatibleSidecar],
    status: &DvAdapterStatus,
) {
    for sidecar in sidecars {
        if status.invocation_succeeded {
            sidecar.proprietary_bitstream_generated = true;
            sidecar.compatibility =
                interoperability::DolbyVisionCompatibilityMetadata::proprietary_adapter_packaged();
            sidecar.notes.push(
                "Proprietary DV adapter validated and emitted certified-compatible output"
                    .to_string(),
            );
            continue;
        }

        sidecar.proprietary_bitstream_generated = false;
        let mut compatibility =
            interoperability::DolbyVisionCompatibilityMetadata::open_sidecar_only();
        compatibility.proprietary_packer_available = status.command_detected;
        if !status.missing_capabilities.is_empty() {
            compatibility.missing_capabilities = status.missing_capabilities.clone();
        }
        sidecar.compatibility = compatibility;
        if let Some(error) = status.error.as_ref() {
            sidecar
                .notes
                .push(format!("DV adapter unavailable: {error}"));
        }
    }
}

fn file_is_non_empty(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Splits an adapter command template into argv tokens and substitutes the
/// `{sidecar}`, `{rpu}`, `{report}` placeholders into each token *after*
/// splitting.
///
/// Substituting after splitting is what keeps path values with spaces or
/// shell metacharacters from re-introducing token boundaries: each placeholder
/// expands inside a single argv element and is then passed directly to
/// `Command::args` without any shell parsing.
///
/// The splitter is `shell_words::split` (POSIX shell quoting rules), so
/// templates can quote tokens that themselves contain spaces — e.g.
/// `"C:/Program Files/dv-tool/dv-pack" {sidecar} --report {report}` parses
/// to four argv elements even though the program path has a space. Without
/// quote support, plain `split_whitespace` would have broken such templates,
/// which is the N-2 follow-up addressed here.
fn split_and_expand_dv_argv(
    template: &str,
    sidecar_path: &Path,
    rpu_out: &Path,
    report_out: &Path,
) -> Result<Vec<String>, String> {
    // P3-I3: NUL bytes in any path placeholder would be passed verbatim
    // into `Command::args`, which `std` rejects with a confusing OS error.
    // Reject them up front with a clear, actionable message instead.
    reject_nul_in_path("sidecar", sidecar_path)?;
    reject_nul_in_path("rpu", rpu_out)?;
    reject_nul_in_path("report", report_out)?;

    let sidecar = sidecar_path.to_string_lossy();
    let rpu = rpu_out.to_string_lossy();
    let report = report_out.to_string_lossy();
    let raw_tokens = shell_words::split(template)
        .map_err(|e| format!("malformed DV adapter command template: {e}"))?;
    Ok(raw_tokens
        .into_iter()
        .map(|token| {
            token
                .replace("{sidecar}", &sidecar)
                .replace("{rpu}", &rpu)
                .replace("{report}", &report)
        })
        .collect())
}

fn reject_nul_in_path(label: &'static str, path: &Path) -> Result<(), String> {
    if path.as_os_str().to_string_lossy().contains('\0') {
        return Err(format!(
            "{label} path '{}' contains a NUL byte; refusing to spawn",
            path.display()
        ));
    }
    Ok(())
}

fn run_dv_adapter(
    dv_tool_cmd: Option<&str>,
    sidecar_path: &Path,
    rpu_out: &Path,
    report_out: &Path,
) -> DvAdapterStatus {
    // The adapter contract is best-effort: we always emit open artefacts and
    // then optionally enrich them when a proprietary bridge succeeds.
    let Some(cmd) = dv_tool_cmd.map(str::trim).filter(|v| !v.is_empty()) else {
        return DvAdapterStatus {
            configured_command: None,
            command_detected: false,
            invocation_attempted: false,
            invocation_succeeded: false,
            rpu_output_path: None,
            missing_capabilities: dv_missing_capabilities(),
            error: Some(
                "No DV adapter command configured; generated open-compatible sidecar only"
                    .to_string(),
            ),
        };
    };

    // Parse the command template into an argv vector and substitute placeholders
    // into individual elements. We deliberately do NOT invoke a shell here:
    // passing paths through `sh -c` / `cmd /C` would let shell metacharacters in
    // the output directory escape into arbitrary command execution.
    let argv = match split_and_expand_dv_argv(cmd, sidecar_path, rpu_out, report_out) {
        Ok(argv) => argv,
        Err(err) => {
            return DvAdapterStatus {
                configured_command: Some(cmd.to_string()),
                command_detected: false,
                invocation_attempted: false,
                invocation_succeeded: false,
                rpu_output_path: None,
                missing_capabilities: dv_missing_capabilities(),
                error: Some(err),
            };
        }
    };
    if argv.is_empty() {
        return DvAdapterStatus {
            configured_command: Some(cmd.to_string()),
            command_detected: false,
            invocation_attempted: false,
            invocation_succeeded: false,
            rpu_output_path: None,
            missing_capabilities: dv_missing_capabilities(),
            error: Some("DV adapter command became empty after placeholder expansion".to_string()),
        };
    }
    let program = &argv[0];
    let args = &argv[1..];

    // GG-6: scrub QDRV_SIGNING_KEY from the child's environment before
    // spawning the (untrusted) external DV adapter. The parent process
    // legitimately reads the env var (via clap's `#[arg(env = ...)]`
    // wiring on the manifest/conformance commands) and would otherwise
    // pass it through to every child it forks. The DV adapter has no
    // legitimate use for the QDRV signing key, so we strip it from the
    // inherited environment.
    let output = Command::new(program)
        .args(args)
        .env_remove("QDRV_SIGNING_KEY")
        .output();

    match output {
        Err(err) => DvAdapterStatus {
            configured_command: Some(cmd.to_string()),
            command_detected: false,
            invocation_attempted: true,
            invocation_succeeded: false,
            rpu_output_path: None,
            missing_capabilities: dv_missing_capabilities(),
            error: Some(format!("failed executing DV adapter command: {err}")),
        },
        Ok(out) => {
            let mut status = DvAdapterStatus {
                configured_command: Some(cmd.to_string()),
                command_detected: true,
                invocation_attempted: true,
                invocation_succeeded: false,
                rpu_output_path: Some(rpu_out.display().to_string()),
                missing_capabilities: Vec::new(),
                error: None,
            };
            if !out.status.success() {
                status.invocation_succeeded = false;
                status.missing_capabilities = dv_missing_capabilities();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let details = if !stderr.is_empty() { stderr } else { stdout };
                status.error = Some(format!(
                    "DV adapter command failed (status={}) program=`{}` argv={:?}: {}",
                    out.status, program, args, details
                ));
                return status;
            }

            if !file_is_non_empty(rpu_out) {
                // Audit LOW (`AUDIT_REPORT_2026-05-27_2311.md`): substitute
                // the actual `rpu_out` path into the diagnostic instead of
                // leaking the `{rpu}` template placeholder token to the
                // operator, which is unactionable during incident triage.
                status.missing_capabilities = dv_missing_capabilities();
                status.error = Some(format!(
                    "DV adapter reported success but did not emit non-empty RPU output at '{}'",
                    rpu_out.display()
                ));
                status.invocation_succeeded = false;
                return status;
            }

            if !file_is_non_empty(report_out) {
                // Same fix as the RPU branch above for the report path.
                status.missing_capabilities = dv_missing_capabilities();
                status.error = Some(format!(
                    "DV adapter reported success but did not emit non-empty report output at '{}'",
                    report_out.display()
                ));
                status.invocation_succeeded = false;
                return status;
            }

            status.invocation_succeeded = true;
            status
        }
    }
}

fn merge_reports(base: &InteropLossReport, other: &InteropLossReport) -> InteropLossReport {
    InteropLossReport {
        target: base.target,
        dropped_fields: union_lists(&base.dropped_fields, &other.dropped_fields),
        approximated_fields: union_lists(&base.approximated_fields, &other.approximated_fields),
        unsupported_features: union_lists(&base.unsupported_features, &other.unsupported_features),
    }
}

fn union_lists(left: &[String], right: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for value in left {
        set.insert(value.clone());
    }
    for value in right {
        set.insert(value.clone());
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufWriter,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use qdrv_codec::Av1Config;
    use qdrv_io::writer::{DeliveryFrame, write_delivery_file};
    use qdrv_meta::{DynamicMeta, StaticMeta};

    #[test]
    fn union_lists_merges_without_duplicates() {
        let a = vec!["x".to_string(), "y".to_string()];
        let b = vec!["y".to_string(), "z".to_string()];
        let out = union_lists(&a, &b);
        assert_eq!(out, vec!["x".to_string(), "y".to_string(), "z".to_string()]);
    }

    #[test]
    fn split_and_expand_keeps_shell_metacharacters_in_single_argv_element() {
        // A path containing shell metacharacters must NOT introduce extra
        // argv tokens. If split_and_expand_dv_argv treated this as a shell
        // template (the H-1 vulnerability), the `;` would terminate the
        // first command and start a second.
        let evil = Path::new("/tmp/danger; rm -rf ~/interop.dv-compatible.json");
        let benign = Path::new("/tmp/rpu.bin");
        let argv = split_and_expand_dv_argv(
            "mytool --sidecar {sidecar} --rpu {rpu}",
            evil,
            benign,
            benign,
        )
        .expect("template parses cleanly");
        // Tokens: ["mytool", "--sidecar", "<evil path>", "--rpu", "<benign path>"]
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[0], "mytool");
        assert_eq!(argv[1], "--sidecar");
        assert_eq!(argv[2], evil.to_string_lossy());
        assert_eq!(argv[3], "--rpu");
        assert_eq!(argv[4], benign.to_string_lossy());
        // Crucially, no token equals `rm` or `-rf` — those characters live
        // inside the third element as opaque bytes.
        assert!(!argv.iter().any(|t| t == "rm"));
        assert!(!argv.iter().any(|t| t == "-rf"));
    }

    #[test]
    fn split_and_expand_supports_quoted_program_path_with_spaces() {
        // Regression test for N-2: shell_words quoting lets users represent
        // a program path that itself contains a space without inflating it
        // into multiple argv tokens. Previously, `split_whitespace` would
        // have turned "C:/Program Files/..." into two broken tokens.
        let any = Path::new("/tmp/x");
        let argv = split_and_expand_dv_argv(
            r#""C:/Program Files/dv-tool/dv-pack" --sidecar {sidecar}"#,
            any,
            any,
            any,
        )
        .expect("quoted template parses cleanly");
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], "C:/Program Files/dv-tool/dv-pack");
        assert_eq!(argv[1], "--sidecar");
        assert_eq!(argv[2], any.to_string_lossy());
    }

    #[test]
    fn split_and_expand_rejects_unbalanced_quotes() {
        let any = Path::new("/tmp/x");
        let err = split_and_expand_dv_argv(r#"mytool "unterminated"#, any, any, any).unwrap_err();
        assert!(err.contains("malformed"), "unexpected error: {err}");
    }

    #[test]
    fn export_interop_bundle_emits_artifacts_and_reports_without_adapter() {
        let root = make_temp_dir("interop-open");
        let input = root.join("in.qdrv32");
        let output = root.join("out");
        write_delivery_fixture(&input, 2);

        let summary =
            export_interop_bundle(&input, &output, None).expect("interop export should pass");

        assert_eq!(summary.frame_count, 2);
        assert!(file_is_non_empty(Path::new(&summary.hdr10_raw_path)));
        assert!(file_is_non_empty(Path::new(&summary.hdr10plus_json_path)));
        assert!(file_is_non_empty(Path::new(&summary.dv_sidecar_path)));
        assert!(file_is_non_empty(Path::new(&summary.loss_report_path)));
        assert!(file_is_non_empty(Path::new(
            &summary.dv_adapter_report_path
        )));
        assert!(!summary.dv_adapter_status.invocation_succeeded);

        let sidecars: Vec<interoperability::DolbyVisionCompatibleSidecar> = serde_json::from_str(
            &fs::read_to_string(&summary.dv_sidecar_path).expect("read sidecar"),
        )
        .expect("parse sidecar");
        assert_eq!(sidecars.len(), 2);
        assert!(sidecars.iter().all(|s| !s.proprietary_bitstream_generated));
        assert!(sidecars.iter().all(|s| {
            matches!(
                s.compatibility.mode,
                interoperability::DolbyVisionCompatibilityMode::OpenSidecarOnly
            )
        }));
        assert!(
            sidecars
                .iter()
                .all(|s| !s.compatibility.missing_capabilities.is_empty())
        );

        let combined: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&summary.loss_report_path).expect("read loss report"),
        )
        .expect("parse loss report");
        assert_eq!(combined["frame_count"].as_u64(), Some(2));
        assert_eq!(
            combined["dv_adapter_status"]["invocation_succeeded"].as_bool(),
            Some(false)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn export_interop_bundle_runs_adapter_and_sets_certified_mode() {
        let root = make_temp_dir("interop-adapter");
        let input = root.join("in.qdrv32");
        let output = root.join("out");
        write_delivery_fixture(&input, 1);

        // The DV adapter is invoked via Command::new (no shell). Each platform
        // ships a small script that takes the RPU and report output paths as
        // positional arguments so we don't depend on shell metacharacter
        // handling for substitution — which is exactly the vulnerability the
        // new run_dv_adapter avoids.
        #[cfg(target_os = "windows")]
        let cmd = {
            let adapter_script = root.join("dv-adapter.ps1");
            fs::write(
                &adapter_script,
                "param([string]$rpu,[string]$report)\n\
                 $ErrorActionPreference = 'Stop'\n\
                 Set-Content -LiteralPath $rpu -Value 'DV-RPU'\n\
                 Set-Content -LiteralPath $report -Value 'adapter-ok'\n",
            )
            .expect("write powershell adapter script");
            // Single-quote the script path so shell_words treats backslashes
            // in the Windows path literally instead of as POSIX escape
            // characters. This is the documented user pattern that the N-2
            // quote-aware parsing exists to support.
            format!(
                "powershell -NoProfile -ExecutionPolicy Bypass -File '{}' {{rpu}} {{report}}",
                adapter_script.display()
            )
        };
        #[cfg(not(target_os = "windows"))]
        let cmd = {
            let adapter_script = root.join("dv-adapter.sh");
            fs::write(
                &adapter_script,
                "#!/bin/sh\nprintf 'DV-RPU\\n' > \"$1\"\nprintf 'adapter-ok\\n' > \"$2\"\n",
            )
            .expect("write shell adapter script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&adapter_script).unwrap().permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&adapter_script, perms).unwrap();
            }
            format!("sh {} {{rpu}} {{report}}", adapter_script.display())
        };

        let summary =
            export_interop_bundle(&input, &output, Some(&cmd)).expect("interop export should pass");
        assert!(
            summary.dv_adapter_status.invocation_succeeded,
            "unexpected adapter status: {:?}",
            summary.dv_adapter_status
        );
        assert!(file_is_non_empty(Path::new(
            &summary.dv_adapter_report_path
        )));

        let rpu_path = summary
            .dv_adapter_status
            .rpu_output_path
            .clone()
            .expect("rpu path should be present");
        assert!(file_is_non_empty(Path::new(&rpu_path)));

        let sidecars: Vec<interoperability::DolbyVisionCompatibleSidecar> = serde_json::from_str(
            &fs::read_to_string(&summary.dv_sidecar_path).expect("read sidecar"),
        )
        .expect("parse sidecar");
        assert_eq!(sidecars.len(), 1);
        assert!(sidecars.iter().all(|s| s.proprietary_bitstream_generated));
        assert!(sidecars.iter().all(|s| {
            matches!(
                s.compatibility.mode,
                interoperability::DolbyVisionCompatibilityMode::ProprietaryAdapterPackaged
            )
        }));
        assert!(
            sidecars
                .iter()
                .all(|s| s.compatibility.certified_output_generated)
        );

        let _ = fs::remove_dir_all(&root);
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
        let width = 2_u32;
        let height = 2_u32;
        let pixels = vec![Pixel32::new_unchecked(0.1, 0.2, 0.3); (width * height) as usize];
        let mut static_meta = StaticMeta::default_delivery(1000.0, 300.0);
        static_meta.metadata_schema_version = qdrv_meta::compatibility::METADATA_SCHEMA_V1;
        let mut frames = Vec::with_capacity(frame_count as usize);
        for idx in 0..frame_count {
            let mut dynamic = DynamicMeta::new(idx, 1000.0 + idx as f32 * 25.0, 200.0);
            dynamic.metadata_schema_version = static_meta.metadata_schema_version;
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
        writer.flush().expect("flush fixture");
    }
}
