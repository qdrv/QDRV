// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Fidelity evaluation backends used by the `qdrv` CLI.
//!
//! This module computes in-repo metrics (PSNR/SSIM/DeltaE76) and optionally
//! supplements them with VMAF-HDR through an external backend when available.
//! If no external backend is configured or usable, it falls back to a
//! deterministic approximation so conformance checks remain reproducible.

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use qdrv_core::{fidelity::FrameFidelityMetrics, metrics_for_delivery_frame, pixel::Pixel32};
use qdrv_meta::{FidelityContract, MeasuredFidelity};
use serde_json::Value;

const CUSTOM_VMAF_CMD_ENV: &str = "QDRV_VMAF_HDR_CMD";
const CUSTOM_VMAF_MODEL_ENV: &str = "QDRV_VMAF_HDR_MODEL";

static FFMPEG_LIBVMAF_AVAILABLE: OnceLock<bool> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct FidelityMeasurement {
    pub(crate) measured: MeasuredFidelity,
    pub(crate) backend_notes: Vec<String>,
}

pub(crate) fn measure_fidelity(
    reference: &[Pixel32],
    candidate: &[Pixel32],
    width: u32,
    height: u32,
    frame_index: u64,
    contract: &FidelityContract,
    allow_vmaf_approximation: bool,
) -> Result<FidelityMeasurement, String> {
    let metrics = metrics_for_delivery_frame(reference, candidate)
        .ok_or_else(|| "fidelity metrics could not be computed".to_string())?;

    let mut notes = Vec::new();
    let mut measured = MeasuredFidelity {
        psnr_db: Some(metrics.psnr_db),
        ssim: Some(metrics.ssim),
        delta_e: Some(metrics.delta_e76),
        vmaf_hdr: None,
    };

    if contract.vmaf_hdr_min.is_some() {
        // Audit MEDIUM (`AUDIT_REPORT_28-05-2026_2053.md`): the previous
        // implementation unconditionally recorded `Some(score)` even when
        // the score was produced by the deterministic surrogate, which
        // let a contract gate that asked for "real VMAF-HDR ≥ 85" pass on
        // the in-repo approximation. `evaluate_vmaf_hdr` now returns
        // `Option<f64>` and the surrogate score only fills the
        // `measured.vmaf_hdr` slot when the operator has explicitly
        // opted in via `QDRV_VMAF_HDR_ALLOW_APPROX=1`. Otherwise the
        // measurement stays `None` and `FidelityContract::evaluate`'s
        // "metric unavailable" branch fails the gate fail-closed.
        let (score, note) = evaluate_vmaf_hdr(
            reference,
            candidate,
            width,
            height,
            frame_index,
            &metrics,
            allow_vmaf_approximation,
        );
        measured.vmaf_hdr = score;
        notes.push(note);
    }

    Ok(FidelityMeasurement {
        measured,
        backend_notes: notes,
    })
}

/// Env var that, when set to `1`/`true`, lets the deterministic VMAF-HDR
/// approximation satisfy a `vmaf_hdr_min` fidelity contract. Read once by
/// the CLI dispatcher and passed through to the library functions as a
/// `bool`, so the library remains free of global state and is testable
/// without env mutation. Audit MEDIUM
/// (`AUDIT_REPORT_28-05-2026_2053.md`).
pub(crate) const VMAF_HDR_ALLOW_APPROX_ENV: &str = "QDRV_VMAF_HDR_ALLOW_APPROX";

/// Reads `QDRV_VMAF_HDR_ALLOW_APPROX` once and returns `true` if it is
/// set to `1` or `true` (case-insensitive). Used by `main.rs` to populate
/// the `allow_approximation` parameter that flows into `measure_fidelity`.
pub(crate) fn vmaf_hdr_approximation_allowed_from_env() -> bool {
    match env::var(VMAF_HDR_ALLOW_APPROX_ENV) {
        Ok(value) => {
            let trimmed = value.trim();
            matches!(trimmed, "1") || trimmed.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

fn evaluate_vmaf_hdr(
    reference: &[Pixel32],
    candidate: &[Pixel32],
    width: u32,
    height: u32,
    frame_index: u64,
    metrics: &FrameFidelityMetrics,
    allow_approx: bool,
) -> (Option<f64>, String) {
    // Backend selection order is intentionally strict so explicit user wiring
    // wins over auto-detection.
    //
    // Audit MEDIUM (`AUDIT_REPORT_28-05-2026_2053.md`): every path that
    // would previously substitute the deterministic surrogate for a real
    // VMAF score now consults the `allow_approx` parameter. When the
    // operator has not opted in via `QDRV_VMAF_HDR_ALLOW_APPROX=1` (or
    // the equivalent test-side `bool` argument), the function returns
    // `None` for the score so the contract evaluator's "metric
    // unavailable" branch fails the gate fail-closed. The text note
    // still records the reason so operators can see *why* their gate
    // failed.

    fn approx_result(approx: f64, reason: &str, allow: bool) -> (Option<f64>, String) {
        let acceptance = if allow { "accepted" } else { "rejected" };
        let suffix = if allow {
            format!(" score={approx:.4}")
        } else {
            format!(
                " score_withheld=true (set {VMAF_HDR_ALLOW_APPROX_ENV}=1 to accept the surrogate)"
            )
        };
        let note = format!(
            "vmaf_hdr backend=deterministic-approx reason={reason} approximation={acceptance}{suffix}"
        );
        (allow.then_some(approx), note)
    }

    // VMAF's reference implementation analyses 33-pixel-wide elementary
    // metrics windows (the default `vif_kernelscale=1.0`/`adm_csf_array`
    // sliding windows). Frames smaller than 33 pixels on either axis
    // therefore can't be scored by libvmaf at all — running ffmpeg on a
    // < 33×33 frame either errors or produces a meaningless score. We
    // short-circuit to the deterministic approximation in that case;
    // contracts that need a real score on tiny fixtures must set the
    // opt-in env var or drop `vmaf_hdr_min`. Audit finding HH-3 / MEDIUM
    // follow-up.
    const VMAF_MIN_DIMENSION: u32 = 33;
    if width < VMAF_MIN_DIMENSION || height < VMAF_MIN_DIMENSION {
        let approx = deterministic_vmaf_approximation(metrics);
        return approx_result(approx, "dimensions_below_33x33", allow_approx);
    }

    if let Some(template) = custom_vmaf_template() {
        match run_external_vmaf_template(
            reference,
            candidate,
            width,
            height,
            frame_index,
            &template,
        ) {
            Ok(score) => {
                return (
                    Some(score),
                    format!(
                        "vmaf_hdr backend=external-template env={CUSTOM_VMAF_CMD_ENV} score={score:.4}"
                    ),
                );
            }
            Err(err) => {
                let approx = deterministic_vmaf_approximation(metrics);
                let (score, mut note) = approx_result(approx, "template_failed", allow_approx);
                note.push_str(&format!(" error={err}"));
                return (score, note);
            }
        }
    }

    if ffmpeg_libvmaf_available() {
        match run_external_vmaf_ffmpeg(reference, candidate, width, height, frame_index) {
            Ok(score) => {
                return (
                    Some(score),
                    format!("vmaf_hdr backend=ffmpeg-libvmaf score={score:.4}"),
                );
            }
            Err(err) => {
                let approx = deterministic_vmaf_approximation(metrics);
                let (score, mut note) = approx_result(approx, "ffmpeg_failed", allow_approx);
                note.push_str(&format!(" error={err}"));
                return (score, note);
            }
        }
    }

    let approx = deterministic_vmaf_approximation(metrics);
    approx_result(approx, "no_backend_available", allow_approx)
}

fn custom_vmaf_template() -> Option<String> {
    let raw = env::var(CUSTOM_VMAF_CMD_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn ffmpeg_libvmaf_available() -> bool {
    *FFMPEG_LIBVMAF_AVAILABLE.get_or_init(|| {
        let output = Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .env_remove("QDRV_SIGNING_KEY")
            .output();
        let Ok(out) = output else {
            return false;
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let haystack = format!("{stdout}\n{stderr}").to_lowercase();
        haystack.contains("libvmaf")
    })
}

/// RAII guard that removes a temporary workspace directory (and its
/// contents) on drop unless explicitly committed. Used by the VMAF
/// backends so a PPM write, command execution, or JSON parse failure no
/// longer leaks `qdrv-vmaf-*` directories under `std::env::temp_dir`.
///
/// Pattern matches `TempFileGuard` in `main.rs` but recurses via
/// `fs::remove_dir_all` because the workspace holds multiple files.
struct TempDirGuard {
    path: Option<PathBuf>,
}

impl TempDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        // Infallible by type design: `commit(self)` consumes the guard, so
        // any code holding `&self` cannot have called commit. The .expect
        // exists only to satisfy the `Option` discriminant.
        #[allow(clippy::expect_used)]
        self.path
            .as_deref()
            .expect("TempDirGuard accessed after commit (compile-time prevented)")
    }

    /// Suppress the drop-time cleanup; used to keep a successfully-emitted
    /// workspace around for the caller to inspect. Not currently used —
    /// the VMAF backends always want cleanup — but matches the
    /// `TempFileGuard` contract for symmetry.
    #[allow(dead_code)]
    fn commit(mut self) -> PathBuf {
        // Infallible: `new()` is the only constructor and always sets
        // `Some(...)`; `commit(self)` consumes so this can run at most once.
        #[allow(clippy::expect_used)]
        self.path
            .take()
            .expect("TempDirGuard committed twice (compile-time prevented)")
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn run_external_vmaf_ffmpeg(
    reference: &[Pixel32],
    candidate: &[Pixel32],
    width: u32,
    height: u32,
    frame_index: u64,
) -> Result<f64, String> {
    let temp_dir = TempDirGuard::new(create_temp_workspace("qdrv-vmaf-hdr", frame_index)?);
    let ref_path = temp_dir.path().join("ref_000001.ppm");
    let dist_path = temp_dir.path().join("dist_000001.ppm");
    let log_path = temp_dir.path().join("vmaf_log.json");

    write_ppm16(&ref_path, width, height, reference)?;
    write_ppm16(&dist_path, width, height, candidate)?;

    let mut lavfi = format!(
        "libvmaf=log_fmt=json:log_path={}",
        shell_escape_path(&log_path)
    );
    if let Ok(model) = env::var(CUSTOM_VMAF_MODEL_ENV) {
        let m = model.trim();
        if !m.is_empty() {
            lavfi.push_str(&format!(":model=path={m}"));
        }
    }

    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .arg("-i")
        .arg(&ref_path)
        .arg("-i")
        .arg(&dist_path)
        .arg("-lavfi")
        .arg(lavfi)
        .args(["-f", "null", "-"])
        // HH-7: ffmpeg has no need for the QDRV signing key; strip it
        // from the inherited environment so the secret cannot leak to a
        // crash dump, ps listing, or `--print-env` debug surface of an
        // external binary outside our control.
        .env_remove("QDRV_SIGNING_KEY")
        .status()
        .map_err(|e| format!("failed to execute ffmpeg: {e}"))?;

    if !status.success() {
        return Err(format!("ffmpeg exited with status {status}"));
    }

    let score = parse_vmaf_json_score(&log_path)?;
    // temp_dir cleaned up on drop (success path); the explicit
    // `fs::remove_dir_all` from the previous implementation is no longer
    // needed because the guard runs unconditionally.
    Ok(score)
}

fn run_external_vmaf_template(
    reference: &[Pixel32],
    candidate: &[Pixel32],
    width: u32,
    height: u32,
    frame_index: u64,
    template: &str,
) -> Result<f64, String> {
    let temp_dir = TempDirGuard::new(create_temp_workspace("qdrv-vmaf-template", frame_index)?);
    let ref_path = temp_dir.path().join("reference.ppm");
    let dist_path = temp_dir.path().join("candidate.ppm");
    let log_path = temp_dir.path().join("vmaf_log.json");

    write_ppm16(&ref_path, width, height, reference)?;
    write_ppm16(&dist_path, width, height, candidate)?;

    // P3-I3: NUL bytes in any placeholder value would later be rejected by
    // `Command::args` with an opaque OS error. Check up front so the
    // operator gets a clear message naming the offending placeholder.
    reject_nul_in_template_path("ref", &ref_path)?;
    reject_nul_in_template_path("dist", &dist_path)?;
    reject_nul_in_template_path("log", &log_path)?;

    // Parse the template with POSIX shell-quote semantics first, then
    // substitute placeholders into each parsed argv element. This lets
    // operators put quoted tokens that themselves contain spaces (e.g.
    // `"/path with spaces/vmaf-bin" --in {ref}`) into `QDRV_VMAF_HDR_CMD`
    // and keeps placeholder values from re-splitting even when they
    // contain whitespace. N-2 follow-up.
    let raw_tokens = shell_words::split(template)
        .map_err(|e| format!("malformed QDRV_VMAF_HDR_CMD template: {e}"))?;
    let mut tokens = raw_tokens.into_iter().map(|t| {
        t.replace("{ref}", &ref_path.to_string_lossy())
            .replace("{dist}", &dist_path.to_string_lossy())
            .replace("{log}", &log_path.to_string_lossy())
            .replace("{width}", &width.to_string())
            .replace("{height}", &height.to_string())
    });
    let program = tokens
        .next()
        .ok_or_else(|| "QDRV_VMAF_HDR_CMD is empty after expansion".to_string())?;
    let args: Vec<String> = tokens.collect();
    let output = Command::new(&program)
        .args(&args)
        .env_remove("QDRV_SIGNING_KEY") // HH-7: see ffmpeg invocation above.
        .output()
        .map_err(|e| format!("failed to execute custom VMAF command: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "custom VMAF command failed: status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let score = if log_path.exists() {
        parse_vmaf_json_score(&log_path)?
    } else {
        parse_score_from_text(&String::from_utf8_lossy(&output.stdout))
            .or_else(|| parse_score_from_text(&String::from_utf8_lossy(&output.stderr)))
            .ok_or_else(|| {
                // Audit LOW (`AUDIT_REPORT_28-05-2026_0102.md`): substitute
                // the actual `log_path` into the diagnostic instead of
                // leaking the `{log}` template placeholder token to the
                // operator. Same pattern as the `{rpu}` / `{report}` fix
                // in `interop_export.rs`.
                format!(
                    "custom command produced no log file at '{}' and no parseable numeric score",
                    log_path.display()
                )
            })?
    };

    // temp_dir cleaned up on drop (success path); previously this was an
    // explicit `fs::remove_dir_all` call that wouldn't run on the early-
    // return error paths above (N-1 follow-up).
    Ok(score)
}

fn parse_vmaf_json_score(path: &Path) -> Result<f64, String> {
    let data = fs::read_to_string(path)
        .map_err(|e| format!("failed reading VMAF log '{}': {e}", path.display()))?;
    let value: Value =
        serde_json::from_str(&data).map_err(|e| format!("invalid VMAF JSON log: {e}"))?;

    let pooled = value
        .pointer("/pooled_metrics/vmaf/mean")
        .and_then(Value::as_f64);
    if let Some(score) = pooled {
        return Ok(score);
    }

    let frame0 = value
        .pointer("/frames/0/metrics/vmaf")
        .and_then(Value::as_f64);
    if let Some(score) = frame0 {
        return Ok(score);
    }

    Err("could not locate vmaf score in JSON log".to_string())
}

/// Extracts a VMAF score from arbitrary backend command output.
///
/// Audit history: `AUDIT_REPORT_2026-05-27_2311.md` (first-token bug),
/// `AUDIT_REPORT_28-05-2026_0042.md` (`libvmaf` banner anchoring bug),
/// `AUDIT_REPORT_28-05-2026_0117.md` (whole-text last-in-range
/// fallback was fail-open under unrelated trailing numbers).
///
/// The parser now applies three ordered strategies, each one strictly
/// anchored or strictly self-describing:
///
/// 1. **`vmaf score` marker** — the canonical libvmaf/ffmpeg output
///    line is `VMAF score: <N>`. We `rfind` (last occurrence) so any
///    summary line later in the output wins over an earlier mention.
///
/// 2. **`vmaf` marker fallback** — if the stronger marker isn't found,
///    `rfind` plain `vmaf` and take the first in-range number after it.
///    Still `rfind` (not first occurrence) so a `libvmaf 4.0` banner
///    cannot mislead the parser when the real score line comes later.
///
/// 3. **Bare-score input** — the entire trimmed output parses cleanly
///    as a single in-range float. This preserves the legitimate case
///    of a diagnostic backend that emits `87.5\n` and exits, while
///    rejecting outputs that mix unrelated numbers without any `vmaf`
///    marker. Strictly closes the fail-open path that previously
///    accepted the last in-range numeric token across the whole text.
///
/// Every candidate is range-checked against the VMAF definition domain
/// `[0.0, 100.0]` so resolutions, frame counts, and similar out-of-band
/// integers cannot pass as scores under any strategy. When no strategy
/// finds an acceptable score, the function returns `None` — callers
/// surface this as an explicit error rather than silently using a
/// fabricated value.
fn parse_score_from_text(text: &str) -> Option<f64> {
    fn is_score_in_range(value: f64) -> bool {
        value.is_finite() && (0.0..=100.0).contains(&value)
    }

    fn parse_numbers_in(slice: &str) -> impl Iterator<Item = f64> + '_ {
        slice
            .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<f64>().ok())
    }

    // The "vmaf" marker is ASCII; lowercasing the whole input is safe and
    // lets us search case-insensitively. Numeric tokens are ASCII too, so
    // parsing from the lowercased copy does not change results.
    let lower = text.to_ascii_lowercase();

    // Strategy 1: canonical `vmaf score` marker.
    const STRONG_MARKER: &str = "vmaf score";
    if let Some(idx) = lower.rfind(STRONG_MARKER) {
        let after = &lower[idx + STRONG_MARKER.len()..];
        if let Some(score) = parse_numbers_in(after).find(|v| is_score_in_range(*v)) {
            return Some(score);
        }
    }

    // Strategy 2: weaker `vmaf` marker, last occurrence.
    const WEAK_MARKER: &str = "vmaf";
    if let Some(idx) = lower.rfind(WEAK_MARKER) {
        let after = &lower[idx + WEAK_MARKER.len()..];
        if let Some(score) = parse_numbers_in(after).find(|v| is_score_in_range(*v)) {
            return Some(score);
        }
    }

    // Strategy 3: bare-score input. The entire trimmed payload must
    // parse cleanly as a single float in range. Multi-token inputs
    // without a `vmaf` marker are rejected here — that's the audit
    // MEDIUM fix from `AUDIT_REPORT_28-05-2026_0117.md`.
    let trimmed = text.trim();
    if let Ok(value) = trimmed.parse::<f64>()
        && is_score_in_range(value)
    {
        return Some(value);
    }

    None
}

fn deterministic_vmaf_approximation(metrics: &FrameFidelityMetrics) -> f64 {
    let psnr_score = if metrics.psnr_db.is_infinite() {
        100.0
    } else {
        ((metrics.psnr_db - 20.0) / 30.0 * 100.0).clamp(0.0, 100.0)
    };
    let ssim_score = (metrics.ssim.clamp(0.0, 1.0) * 100.0).clamp(0.0, 100.0);
    let delta_e_score = (100.0 - (metrics.delta_e76 * 6.0)).clamp(0.0, 100.0);
    (psnr_score * 0.45 + ssim_score * 0.40 + delta_e_score * 0.15).clamp(0.0, 100.0)
}

fn create_temp_workspace(prefix: &str, frame_index: u64) -> Result<PathBuf, String> {
    let base = env::temp_dir();
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_micros();
    let dir = base.join(format!(
        "{prefix}-{frame_index}-{micros}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create temp workspace '{}': {e}", dir.display()))?;
    Ok(dir)
}

fn write_ppm16(path: &Path, width: u32, height: u32, pixels: &[Pixel32]) -> Result<(), String> {
    let expected = usize::try_from(
        u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| "width × height overflow writing VMAF frame".to_string())?,
    )
    .map_err(|_| "width × height does not fit usize writing VMAF frame".to_string())?;

    if pixels.len() != expected {
        return Err(format!(
            "VMAF frame pixel mismatch: expected {expected}, got {}",
            pixels.len()
        ));
    }

    let mut file =
        fs::File::create(path).map_err(|e| format!("failed creating '{}': {e}", path.display()))?;
    write!(file, "P6\n{width} {height}\n65535\n")
        .map_err(|e| format!("failed writing PPM header '{}': {e}", path.display()))?;
    for p in pixels {
        let r = (p.r.clamp(0.0, 1.0) * 65535.0).round() as u16;
        let g = (p.g.clamp(0.0, 1.0) * 65535.0).round() as u16;
        let b = (p.b.clamp(0.0, 1.0) * 65535.0).round() as u16;
        file.write_all(&r.to_be_bytes())
            .map_err(|e| format!("failed writing red sample '{}': {e}", path.display()))?;
        file.write_all(&g.to_be_bytes())
            .map_err(|e| format!("failed writing green sample '{}': {e}", path.display()))?;
        file.write_all(&b.to_be_bytes())
            .map_err(|e| format!("failed writing blue sample '{}': {e}", path.display()))?;
    }
    Ok(())
}

/// Escapes a path for ffmpeg's `lavfi` filter-graph argument syntax.
///
/// ffmpeg's `-lavfi` parser treats `:` as the filter option separator and `\`
/// as an escape character. Path values for `log_path=...` therefore have to
/// escape **both**:
///
/// - `\` → `\\` (must be escaped first, otherwise the next pass adds a
///   second `\` to existing back-slashes)
/// - `:` → `\:`
///
/// The previous implementation only escaped `:`. On Windows, where
/// `std::env::temp_dir()` returns paths like
/// `C:\Users\<user>\AppData\Local\Temp\...`, that produced
/// `C\:\Users\<user>\AppData\Local\Temp\...` which ffmpeg's lavfi parser
/// then re-interpreted: every unescaped `\X` was a no-op escape, so the
/// backslashes dropped out and ffmpeg wrote the log to a path resembling
/// `Users<user>AppDataLocalTemp...vmaf_log.json` *relative to the current
/// working directory* — outside the `TempDirGuard`'s scope, never cleaned up.
///
/// This is the root cause of the `Usersce940AppDataLocalTempqdrv-vmaf-hdr-...`
/// debris files that previously accumulated at the workspace root on
/// Windows hosts.
fn shell_escape_path(path: &Path) -> String {
    // Order matters: escape backslashes first, then colons, so that the
    // colon-escape's leading `\` is not subsequently double-escaped.
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', "\\:")
}

fn reject_nul_in_template_path(label: &'static str, path: &Path) -> Result<(), String> {
    if path.as_os_str().to_string_lossy().contains('\0') {
        return Err(format!(
            "{{{label}}} path '{}' contains a NUL byte; refusing to spawn",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_vmaf_approx_is_reasonable_range() {
        let strong = FrameFidelityMetrics {
            psnr_db: 50.0,
            ssim: 0.995,
            delta_e76: 0.4,
        };
        let weak = FrameFidelityMetrics {
            psnr_db: 25.0,
            ssim: 0.75,
            delta_e76: 8.0,
        };
        let a = deterministic_vmaf_approximation(&strong);
        let b = deterministic_vmaf_approximation(&weak);
        assert!((0.0..=100.0).contains(&a));
        assert!((0.0..=100.0).contains(&b));
        assert!(a > b);
    }

    #[test]
    fn shell_escape_path_escapes_backslashes_and_colons() {
        // Regression test for the HH-1 Windows debris bug: the escape
        // function must escape BOTH `\` and `:` so ffmpeg's lavfi parser
        // sees the path verbatim instead of stripping the backslashes.
        let win_path = Path::new(r"C:\Users\ce940\AppData\Local\Temp\qdrv-vmaf\log.json");
        let escaped = shell_escape_path(win_path);
        // Every original `\` must be doubled.
        assert!(
            escaped.contains(r"\\Users\\ce940\\AppData\\Local\\Temp\\qdrv-vmaf\\log.json"),
            "backslashes not escaped: {escaped}"
        );
        // The `:` after the drive letter must be backslash-escaped.
        assert!(escaped.starts_with(r"C\:"), "colon not escaped: {escaped}");
        // No bare `:` remains anywhere (the only `:` in the input is after
        // `C`, and we must not see it without a leading `\`).
        let bare_colon = escaped
            .char_indices()
            .any(|(i, c)| c == ':' && (i == 0 || !escaped[..i].ends_with('\\')));
        assert!(!bare_colon, "bare colon survives escape: {escaped}");
    }

    #[test]
    fn shell_escape_path_unix_path_keeps_separators() {
        // A POSIX-style path has no `\` and no `:` — should pass through
        // unchanged so the Linux/macOS path is unaffected by the Windows
        // fix.
        let unix_path = Path::new("/tmp/qdrv-vmaf/log.json");
        let escaped = shell_escape_path(unix_path);
        assert_eq!(escaped, "/tmp/qdrv-vmaf/log.json");
    }

    #[test]
    fn parse_vmaf_json_score_prefers_pooled_metric() {
        let dir = create_temp_workspace("qdrv-vmaf-test", 0).unwrap();
        let path = dir.join("log.json");
        fs::write(
            &path,
            r#"{"pooled_metrics":{"vmaf":{"mean":95.125}},"frames":[{"metrics":{"vmaf":91.2}}]}"#,
        )
        .unwrap();
        let score = parse_vmaf_json_score(&path).unwrap();
        assert!((score - 95.125).abs() < 1e-6);
        let _ = fs::remove_dir_all(dir);
    }

    /// Audit MEDIUM regression (`AUDIT_REPORT_2026-05-27_2311.md`): the
    /// parser must prefer the score that follows a `VMAF` marker, even
    /// when other numeric tokens (banner, version, resolution) appear
    /// earlier in the output.
    #[test]
    fn parse_score_anchors_on_vmaf_marker() {
        let text = "ffmpeg version 4.0\nInput: 1920x1080\nVMAF score: 87.234\n";
        let score = parse_score_from_text(text).expect("must locate score");
        assert!((score - 87.234).abs() < 1e-6, "got {score}");
    }

    /// Audit MEDIUM (`AUDIT_REPORT_28-05-2026_0117.md`): when no `vmaf`
    /// marker is present and the output contains multiple unrelated
    /// numeric tokens, the parser must fail closed rather than guess.
    /// The previous implementation returned the last in-range numeric
    /// token (`88.5` here) which was a fail-open path that could
    /// silently pass a fidelity contract when none of the numbers in
    /// the output was actually a score.
    #[test]
    fn parse_score_rejects_unanchored_multi_token_input() {
        let text = "version 4.0\nframes processed: 240\nfinal: 88.5\n";
        assert!(
            parse_score_from_text(text).is_none(),
            "multi-token unanchored input must fail closed"
        );
    }

    /// Bare-score backends that print only a single in-range number
    /// (and exit) are still supported — the strict fallback accepts
    /// inputs whose entire trimmed payload parses cleanly as one
    /// in-range float. This preserves the diagnostic-backend case
    /// while rejecting multi-number unanchored outputs.
    #[test]
    fn parse_score_accepts_bare_in_range_token() {
        for text in ["87.5", "87.5\n", "  87.5  \n", "100", "0"] {
            let score = parse_score_from_text(text)
                .unwrap_or_else(|| panic!("bare-score input {text:?} must parse to a score"));
            assert!(
                is_score_in_range_for_test(score),
                "parsed score {score} for input {text:?} is out of range"
            );
        }
    }

    /// Bare-token inputs outside `[0.0, 100.0]` must still be rejected
    /// (an out-of-range bare token is not a score).
    #[test]
    fn parse_score_rejects_bare_out_of_range_token() {
        for text in ["150", "-5", "1920"] {
            assert!(
                parse_score_from_text(text).is_none(),
                "bare token {text:?} is out of range and must be rejected"
            );
        }
    }

    /// Out-of-range numbers (resolutions, frame counts) must be ignored
    /// even when they are the only numbers present — the parser must
    /// return `None` rather than the first in-range number from
    /// elsewhere, because there is no `vmaf` marker and no bare-score
    /// shape.
    #[test]
    fn parse_score_rejects_resolution_and_frame_counts() {
        let text = "Resolution: 1920x1080\nframes: 7200\n";
        assert!(parse_score_from_text(text).is_none());
    }

    fn is_score_in_range_for_test(value: f64) -> bool {
        value.is_finite() && (0.0..=100.0).contains(&value)
    }

    /// Audit MEDIUM regression (`AUDIT_REPORT_28-05-2026_2053.md`): when
    /// the VMAF-HDR backend is unavailable and the operator has not
    /// opted in via `allow_approx`, `evaluate_vmaf_hdr` must return
    /// `None` so the contract evaluator fails closed via its
    /// "metric unavailable" branch.
    #[test]
    fn evaluate_vmaf_withholds_surrogate_when_not_allowed() {
        // Small frame so the small-frame path is taken — no backend at all.
        let metrics = FrameFidelityMetrics {
            psnr_db: 40.0,
            ssim: 0.95,
            delta_e76: 1.5,
        };
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 16];
        let (score, note) = evaluate_vmaf_hdr(&pixels, &pixels, 4, 4, 0, &metrics, false);
        assert!(
            score.is_none(),
            "surrogate must be withheld when allow_approx=false; got Some({score:?}), note={note}"
        );
        assert!(
            note.contains("approximation=rejected"),
            "note must record rejection, got: {note}"
        );
        assert!(
            note.contains(VMAF_HDR_ALLOW_APPROX_ENV),
            "note must reference the opt-in env var, got: {note}"
        );
    }

    /// Companion to the rejection test: with `allow_approx=true` the
    /// surrogate score is returned as `Some(value)` and the note records
    /// acceptance.
    #[test]
    fn evaluate_vmaf_accepts_surrogate_when_opted_in() {
        let metrics = FrameFidelityMetrics {
            psnr_db: 40.0,
            ssim: 0.95,
            delta_e76: 1.5,
        };
        let pixels = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 16];
        let (score, note) = evaluate_vmaf_hdr(&pixels, &pixels, 4, 4, 0, &metrics, true);
        let score = score.expect("opted-in path must return Some(score)");
        assert!(
            is_score_in_range_for_test(score),
            "surrogate score must be in VMAF range, got {score}"
        );
        assert!(
            note.contains("approximation=accepted"),
            "note must record acceptance, got: {note}"
        );
    }

    /// `measure_fidelity` must propagate `None` into `measured.vmaf_hdr`
    /// when the surrogate is withheld, so the contract evaluator's
    /// `None` branch fires and the gate fails closed.
    #[test]
    fn measure_fidelity_leaves_vmaf_none_when_surrogate_withheld() {
        let metrics_input = vec![Pixel32::new_unchecked(0.5, 0.5, 0.5); 16];
        let contract = FidelityContract {
            psnr_db_min: None,
            ssim_min: None,
            delta_e_max: None,
            vmaf_hdr_min: Some(85.0),
        };
        let measurement =
            measure_fidelity(&metrics_input, &metrics_input, 4, 4, 0, &contract, false)
                .expect("measure_fidelity must succeed even when surrogate is withheld");
        assert!(
            measurement.measured.vmaf_hdr.is_none(),
            "vmaf_hdr must remain None without opt-in"
        );
        // The contract evaluator should now report that no in-repo
        // evaluator is available for VMAF-HDR.
        let result = contract.evaluate(&measurement.measured);
        assert!(!result.passed, "gate must fail closed");
        assert!(
            result.failures.iter().any(|f| f.contains("VMAF-HDR")),
            "failure must cite the missing VMAF-HDR metric, got: {:?}",
            result.failures
        );
    }

    /// A marker followed by an out-of-range value (e.g. a malformed
    /// "VMAF score: 9999") must fall through to the last-in-range
    /// fallback rather than returning the bogus value.
    #[test]
    fn parse_score_skips_out_of_range_score_after_marker() {
        let text = "VMAF score: 9999\nactual: 73.5\n";
        let score = parse_score_from_text(text).expect("fallback must succeed");
        assert!((score - 73.5).abs() < 1e-6, "got {score}");
    }

    /// Audit MEDIUM regression (`AUDIT_REPORT_28-05-2026_0042.md`): a
    /// banner mentioning `libvmaf` followed by a version number is the
    /// realistic ffmpeg failure case. The parser previously anchored on
    /// the *first* `vmaf` token, which sat inside `libvmaf`, and
    /// returned the version `4.0` instead of the real `87.234` score
    /// further down the output. The fix uses `rfind` so the score line
    /// near the end wins over the tool-name banner.
    #[test]
    fn parse_score_prefers_score_line_over_libvmaf_banner() {
        let text = "ffmpeg libvmaf 4.0\nInput: 1920x1080\nVMAF score: 87.234\n";
        let score = parse_score_from_text(text).expect("must locate score");
        assert!(
            (score - 87.234).abs() < 1e-6,
            "expected 87.234 from canonical marker, got {score}"
        );
    }

    /// Companion test: even without the canonical `vmaf score` marker,
    /// a later bare `vmaf` line must win over an earlier `libvmaf`
    /// banner so the parser still anchors on the actual score line.
    #[test]
    fn parse_score_uses_last_vmaf_marker_when_no_score_keyword() {
        let text = "libvmaf 4.0\nVMAF: 88.5\nbuild 2026\n";
        let score = parse_score_from_text(text).expect("must locate score");
        assert!(
            (score - 88.5).abs() < 1e-6,
            "expected 88.5 from last vmaf marker, got {score}"
        );
    }
}
