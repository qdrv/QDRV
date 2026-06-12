// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Open conformance corpus generation and execution helpers.
//!
//! The generator creates deterministic mastering inputs, golden delivery files,
//! metadata signatures, and fidelity contracts. The runner then re-transcodes
//! vectors and checks canonical hashes, metadata signatures, and fidelity
//! thresholds so regressions are caught in a reproducible way.

use std::{
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use qdrv_codec::{Av1Config, ChromaSampling420, MasteringCodec};
use qdrv_core::{pixel::Pixel64, pq::PQ_MAX_NITS};
use qdrv_encode::{EncodeOptions, transcode_frame_with_options};
use qdrv_io::{
    container::TIER_MASTERING,
    reader::QdrvStreamReader,
    writer::{DeliveryFrame, MasteringFrame, write_delivery_file, write_mastering_file},
};
use qdrv_meta::{
    DynamicMeta, FidelityContract, SignedMetadataManifest, StaticMeta, Tier, manifest, sha256_hex,
};
use serde::{Deserialize, Serialize};

use crate::atomic_write;
use crate::fidelity_eval::measure_fidelity;

pub(crate) const CONFORMANCE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConformanceCorpusManifest {
    pub(crate) schema_version: u16,
    pub(crate) corpus_name: String,
    pub(crate) generated_unix_ms: u128,
    pub(crate) vectors: Vec<ConformanceVector>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConformanceVector {
    pub(crate) id: String,
    pub(crate) input_mastering_path: String,
    pub(crate) golden_delivery_path: String,
    pub(crate) expected_delivery_sha256: String,
    #[serde(default)]
    pub(crate) fidelity_contract_path: Option<String>,
    #[serde(default)]
    pub(crate) golden_dynamic_meta_json_path: Option<String>,
    #[serde(default)]
    pub(crate) golden_dynamic_meta_manifest_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConformanceVectorResult {
    pub(crate) id: String,
    pub(crate) passed: bool,
    pub(crate) hash_match: bool,
    pub(crate) signature_verified: bool,
    pub(crate) expected_sha256: String,
    pub(crate) actual_sha256: String,
    #[serde(default)]
    pub(crate) fidelity_notes: Vec<String>,
    #[serde(default)]
    pub(crate) failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConformanceRunSummary {
    pub(crate) schema_version: u16,
    pub(crate) corpus_name: String,
    pub(crate) generated_unix_ms: u128,
    pub(crate) total_vectors: usize,
    pub(crate) passed_vectors: usize,
    pub(crate) failed_vectors: usize,
    pub(crate) all_passed: bool,
    pub(crate) results: Vec<ConformanceVectorResult>,
}

/// Tunable knobs for [`generate_open_vectors`]. Grouping these avoids the
/// `clippy::too_many_arguments` cap when the function signature grows
/// (e.g., to take the audit MEDIUM follow-up `allow_vmaf_approximation`
/// flag), and gives a single place to add future toggles.
pub(crate) struct OpenVectorsConfig {
    pub(crate) vector_count: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// When `true`, the deterministic VMAF-HDR surrogate may satisfy a
    /// `vmaf_hdr_min` contract gate. When `false` (the default), the
    /// surrogate score is withheld so the contract evaluator reports
    /// "metric unavailable" and the gate fails closed. Audit MEDIUM
    /// (`AUDIT_REPORT_28-05-2026_2053.md`).
    pub(crate) allow_vmaf_approximation: bool,
}

pub(crate) fn generate_open_vectors(
    output_dir: &Path,
    corpus_name: &str,
    config: &OpenVectorsConfig,
    signing_key: &[u8],
    signer: &str,
) -> Result<PathBuf, String> {
    let OpenVectorsConfig {
        vector_count,
        width,
        height,
        allow_vmaf_approximation,
    } = *config;
    if width == 0 || height == 0 {
        return Err("open vectors require width/height > 0".to_string());
    }
    if vector_count == 0 {
        return Err("open vectors require at least one vector".to_string());
    }

    fs::create_dir_all(output_dir).map_err(|e| {
        format!(
            "cannot create conformance output directory '{}': {e}",
            output_dir.display()
        )
    })?;

    let mut vectors = Vec::with_capacity(vector_count);
    for idx in 0..vector_count {
        let id = format!("open-vector-{idx:03}");
        // Sanity-check our own generated IDs against the same predicate the
        // runner uses; this would catch any future change to the id format
        // that accidentally introduces a disallowed character.
        validate_vector_id(&id).map_err(|e| format!("internal: generated id rejected: {e}"))?;
        let input_name = format!("{id}.input.qdrv64");
        let golden_name = format!("{id}.golden.qdrv32");
        let dynamic_name = format!("{id}.dynamic-meta.json");
        let dynamic_manifest_name = format!("{id}.dynamic-meta.manifest.json");
        let contract_name = format!("{id}.fidelity-contract.json");

        let input_path = output_dir.join(&input_name);
        let golden_path = output_dir.join(&golden_name);
        let dynamic_path = output_dir.join(&dynamic_name);
        let dynamic_manifest_path = output_dir.join(&dynamic_manifest_name);
        let contract_path = output_dir.join(&contract_name);

        let mastering_pixels = build_mastering_pattern(width, height, idx as u64)?;
        write_open_mastering_file(&input_path, width, height, &mastering_pixels)?;
        let delivery_frames = transcode_mastering_file_deterministic(
            &input_path,
            &golden_path,
            false,
            None,
            allow_vmaf_approximation,
        )?;

        let first_dynamic = delivery_frames
            .first()
            .map(|f| f.dynamic_meta.clone())
            .ok_or_else(|| "generated vector has no frames".to_string())?;
        let dynamic_json = serde_json::to_string_pretty(&first_dynamic)
            .map_err(|e| format!("failed serializing dynamic metadata: {e}"))?;
        // L-1 (`AUDIT_REPORT.md` 2026-05-27): every conformance artefact now
        // goes through the atomic-temp + rename + fsync helper so an
        // interrupted generator run cannot leave a half-written JSON file
        // masquerading as a complete one. Same code path as
        // `cmd_write_test` / `cmd_convert` / `cmd_mux` /
        // `cmd_export_interop`.
        atomic_write(&dynamic_path, dynamic_json.as_bytes()).map_err(|e| {
            format!(
                "failed writing dynamic metadata '{}': {e}",
                dynamic_path.display()
            )
        })?;

        let signed = manifest::sign_manifest(dynamic_json.as_bytes(), signer, signing_key)
            .map_err(|e| format!("failed signing dynamic metadata manifest: {e}"))?;
        let signed_json = serde_json::to_string_pretty(&signed)
            .map_err(|e| format!("failed serializing signed metadata manifest: {e}"))?;
        atomic_write(&dynamic_manifest_path, signed_json.as_bytes()).map_err(|e| {
            format!(
                "failed writing dynamic metadata manifest '{}': {e}",
                dynamic_manifest_path.display()
            )
        })?;

        let fidelity_contract = FidelityContract {
            psnr_db_min: Some(35.0),
            ssim_min: Some(0.90),
            delta_e_max: Some(4.5),
            vmaf_hdr_min: Some(85.0),
        };
        let contract_json = serde_json::to_string_pretty(&fidelity_contract)
            .map_err(|e| format!("failed serializing fidelity contract: {e}"))?;
        atomic_write(&contract_path, contract_json.as_bytes()).map_err(|e| {
            format!(
                "failed writing fidelity contract '{}': {e}",
                contract_path.display()
            )
        })?;

        let expected_hash = canonical_delivery_payload_sha256(&golden_path)?;

        vectors.push(ConformanceVector {
            id,
            input_mastering_path: input_name,
            golden_delivery_path: golden_name,
            expected_delivery_sha256: expected_hash,
            fidelity_contract_path: Some(contract_name),
            golden_dynamic_meta_json_path: Some(dynamic_name),
            golden_dynamic_meta_manifest_path: Some(dynamic_manifest_name),
        });
    }

    let manifest_payload = ConformanceCorpusManifest {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        corpus_name: corpus_name.to_string(),
        generated_unix_ms: unix_ms_now()?,
        vectors,
    };
    let manifest_path = output_dir.join("conformance-manifest.json");
    let manifest_json = serde_json::to_string_pretty(&manifest_payload)
        .map_err(|e| format!("failed serializing conformance manifest: {e}"))?;
    atomic_write(&manifest_path, manifest_json.as_bytes())
        .map_err(|e| format!("failed writing '{}': {e}", manifest_path.display()))?;

    Ok(manifest_path)
}

pub(crate) fn run_conformance(
    manifest_path: &Path,
    output_dir: &Path,
    signing_key: &[u8],
    allow_vmaf_approximation: bool,
) -> Result<ConformanceRunSummary, String> {
    let root = manifest_path.parent().ok_or_else(|| {
        format!(
            "manifest path '{}' has no parent directory",
            manifest_path.display()
        )
    })?;
    let manifest = load_manifest(manifest_path)?;
    if manifest.schema_version != CONFORMANCE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported conformance manifest schema {} (expected {})",
            manifest.schema_version, CONFORMANCE_SCHEMA_VERSION
        ));
    }

    fs::create_dir_all(output_dir).map_err(|e| {
        format!(
            "cannot create conformance output '{}': {e}",
            output_dir.display()
        )
    })?;

    let mut results = Vec::with_capacity(manifest.vectors.len());
    for vector in &manifest.vectors {
        validate_vector_id(&vector.id)
            .map_err(|e| format!("invalid vector id in manifest: {e}"))?;
        let input_path = resolve_rel(root, &vector.input_mastering_path).map_err(|e| {
            format!(
                "invalid input_mastering_path for vector '{}': {e}",
                vector.id
            )
        })?;
        let golden_path = resolve_rel(root, &vector.golden_delivery_path).map_err(|e| {
            format!(
                "invalid golden_delivery_path for vector '{}': {e}",
                vector.id
            )
        })?;
        // vector.id has already been validated, so this filename cannot
        // contain path separators or `..` components.
        let candidate_path = output_dir.join(format!("{}.candidate.qdrv32", vector.id));

        let mut failures = Vec::new();
        let mut fidelity_notes = Vec::new();

        let signature_verified = match (
            vector.golden_dynamic_meta_json_path.as_deref(),
            vector.golden_dynamic_meta_manifest_path.as_deref(),
        ) {
            (Some(dynamic_rel), Some(manifest_rel)) => {
                let dynamic_path = resolve_rel(root, dynamic_rel).map_err(|e| {
                    format!(
                        "invalid golden_dynamic_meta_json_path for '{}': {e}",
                        vector.id
                    )
                })?;
                let manifest_full = resolve_rel(root, manifest_rel).map_err(|e| {
                    format!(
                        "invalid golden_dynamic_meta_manifest_path for '{}': {e}",
                        vector.id
                    )
                })?;
                let payload = fs::read(&dynamic_path).map_err(|e| {
                    format!(
                        "cannot read signed dynamic metadata payload '{}' for '{}': {e}",
                        dynamic_rel, vector.id
                    )
                })?;
                let signed_json = fs::read_to_string(&manifest_full).map_err(|e| {
                    format!(
                        "cannot read signed dynamic metadata manifest '{}' for '{}': {e}",
                        manifest_rel, vector.id
                    )
                })?;
                let signed: SignedMetadataManifest =
                    serde_json::from_str(&signed_json).map_err(|e| {
                        format!(
                            "invalid signed dynamic metadata manifest '{}' for '{}': {e}",
                            manifest_rel, vector.id
                        )
                    })?;
                if let Err(err) = manifest::verify_manifest(&payload, &signed, signing_key) {
                    failures.push(format!("metadata signature verification failed: {err}"));
                    false
                } else {
                    true
                }
            }
            _ => true,
        };

        let transcode_result = transcode_mastering_file_deterministic(
            &input_path,
            &candidate_path,
            true,
            None,
            allow_vmaf_approximation,
        );
        if let Err(err) = transcode_result {
            failures.push(format!("failed transcoding vector '{}': {err}", vector.id));
        }

        let actual_hash = if candidate_path.exists() {
            canonical_delivery_payload_sha256(&candidate_path)?
        } else {
            String::new()
        };
        let hash_match = actual_hash == vector.expected_delivery_sha256;
        if !hash_match {
            failures.push(format!(
                "sha256 mismatch expected={} actual={}",
                vector.expected_delivery_sha256, actual_hash
            ));
        }

        if let Some(contract_rel) = &vector.fidelity_contract_path {
            let contract_path = resolve_rel(root, contract_rel)
                .map_err(|e| format!("invalid fidelity_contract_path for '{}': {e}", vector.id))?;
            let contract_json = fs::read_to_string(&contract_path).map_err(|e| {
                format!(
                    "cannot read fidelity contract '{}' for '{}': {e}",
                    contract_rel, vector.id
                )
            })?;
            let contract: FidelityContract = serde_json::from_str(&contract_json).map_err(|e| {
                format!(
                    "invalid fidelity contract '{}' for '{}': {e}",
                    contract_rel, vector.id
                )
            })?;

            if candidate_path.exists() && golden_path.exists() {
                match evaluate_candidate_vs_golden(
                    &candidate_path,
                    &golden_path,
                    &contract,
                    allow_vmaf_approximation,
                ) {
                    Ok(notes) => fidelity_notes.extend(notes),
                    Err(err) => failures.push(format!("fidelity contract check failed: {err}")),
                }
            } else {
                failures.push("candidate or golden file missing for fidelity check".to_string());
            }
        }

        results.push(ConformanceVectorResult {
            id: vector.id.clone(),
            passed: failures.is_empty(),
            hash_match,
            signature_verified,
            expected_sha256: vector.expected_delivery_sha256.clone(),
            actual_sha256: actual_hash,
            fidelity_notes,
            failures,
        });
    }

    let passed = results.iter().filter(|r| r.passed).count();
    let summary = ConformanceRunSummary {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        corpus_name: manifest.corpus_name,
        generated_unix_ms: unix_ms_now()?,
        total_vectors: results.len(),
        passed_vectors: passed,
        failed_vectors: results.len().saturating_sub(passed),
        all_passed: passed == results.len(),
        results,
    };

    let summary_path = output_dir.join("conformance-summary.json");
    let summary_json = serde_json::to_string_pretty(&summary)
        .map_err(|e| format!("failed serializing conformance summary: {e}"))?;
    atomic_write(&summary_path, summary_json.as_bytes())
        .map_err(|e| format!("failed writing '{}': {e}", summary_path.display()))?;

    Ok(summary)
}

fn evaluate_candidate_vs_golden(
    candidate_path: &Path,
    golden_path: &Path,
    contract: &FidelityContract,
    allow_vmaf_approximation: bool,
) -> Result<Vec<String>, String> {
    let mut cand_stream =
        QdrvStreamReader::new(BufReader::new(File::open(candidate_path).map_err(|e| {
            format!("cannot open candidate '{}': {e}", candidate_path.display())
        })?))
        .map_err(|e| format!("cannot parse candidate '{}': {e}", candidate_path.display()))?;
    let mut golden_stream =
        QdrvStreamReader::new(BufReader::new(File::open(golden_path).map_err(|e| {
            format!("cannot open golden '{}': {e}", golden_path.display())
        })?))
        .map_err(|e| format!("cannot parse golden '{}': {e}", golden_path.display()))?;

    let cand_frame = cand_stream
        .next_frame()
        .map_err(|e| format!("cannot read candidate frame: {e}"))?
        .ok_or_else(|| "candidate file has no frames".to_string())?;
    let golden_frame = golden_stream
        .next_frame()
        .map_err(|e| format!("cannot read golden frame: {e}"))?
        .ok_or_else(|| "golden file has no frames".to_string())?;

    let cand_pixels = cand_frame
        .pixels
        .as_delivery()
        .ok_or_else(|| "candidate frame is not delivery-tier".to_string())?;
    let golden_pixels = golden_frame
        .pixels
        .as_delivery()
        .ok_or_else(|| "golden frame is not delivery-tier".to_string())?;

    let measurement = measure_fidelity(
        golden_pixels,
        cand_pixels,
        cand_stream.header().width,
        cand_stream.header().height,
        0,
        contract,
        allow_vmaf_approximation,
    )?;

    let eval = contract.evaluate(&measurement.measured);
    if !eval.passed {
        return Err(eval.failures.join("; "));
    }
    Ok(measurement.backend_notes)
}

fn load_manifest(path: &Path) -> Result<ConformanceCorpusManifest, String> {
    let data =
        fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
    serde_json::from_str(&data)
        .map_err(|e| format!("invalid conformance manifest '{}': {e}", path.display()))
}

/// Joins `rel` onto `root` only if `rel` is a safe forward-only relative
/// path: no absolute root, no `..` parent traversal, no Windows path prefix.
///
/// This is the defensive parser used for every path string read from a
/// conformance manifest, so a tampered manifest cannot direct read or write
/// operations outside the manifest's own directory tree.
fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => continue,
            Component::ParentDir => {
                return Err(format!("manifest path '{rel}' escapes its root via '..'"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("manifest path '{rel}' must be relative"));
            }
        }
    }
    Ok(root.join(candidate))
}

/// Returns Ok only for vector IDs composed of ASCII alphanumerics, `-`, or
/// `_`, with length 1..=128. Used to validate `ConformanceVector::id` before
/// it is interpolated into output filenames so a malicious manifest cannot
/// inject path separators or hidden-file dots.
fn validate_vector_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err(format!("vector id '{id}' must be 1..=128 characters long"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "vector id '{id}' must contain only ASCII alphanumerics, '-' or '_'"
        ));
    }
    Ok(())
}

fn build_mastering_pattern(width: u32, height: u32, seed: u64) -> Result<Vec<Pixel64>, String> {
    let count = usize::try_from(
        u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| "width × height overflow building mastering pattern".to_string())?,
    )
    .map_err(|_| "width × height does not fit usize".to_string())?;

    let mut pixels = Vec::with_capacity(count);
    for i in 0..count {
        let x = (i % width as usize) as f64 / (width.saturating_sub(1)).max(1) as f64;
        let y = (i / width as usize) as f64 / (height.saturating_sub(1)).max(1) as f64;
        let ripple =
            (((seed as f64 + 1.0) * (x * 17.0 + y * 13.0)).sin() * 0.5 + 0.5).clamp(0.0, 1.0);
        let peak = 1_200.0 + (seed as f64 * 75.0);
        let r = (x * peak + ripple * 120.0).clamp(0.0, PQ_MAX_NITS);
        let g = (y * peak + ripple * 90.0).clamp(0.0, PQ_MAX_NITS);
        let b = ((x * 0.6 + y * 0.4) * peak + ripple * 60.0).clamp(0.0, PQ_MAX_NITS);
        pixels.push(Pixel64::new_unchecked(r, g, b));
    }
    Ok(pixels)
}

/// Writes a single-frame mastering input file for an open-vector entry.
///
/// The generated file always contains exactly one frame, so its
/// `dynamic_meta.frame_index` is hardcoded to `0` to satisfy the reader's
/// P3-3 sequencing contract (`frame_index == position`). Pixel-content
/// variation between vectors is provided upstream by
/// [`build_mastering_pattern`], which seeds the pattern on the vector
/// index; this writer just persists the result.
///
/// This was the P6-1 regression site: previously the caller's vector
/// index was passed as `frame_index`, which after P3-3 caused the
/// just-written file to be rejected on read-back for any vector beyond
/// the first.
fn write_open_mastering_file(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[Pixel64],
) -> Result<(), String> {
    let static_meta = StaticMeta::default_mastering();
    let frame = MasteringFrame {
        dynamic_meta: DynamicMeta::new(0, 1_200.0, 240.0),
        pixels: pixels.to_vec(),
    };
    let mut writer = BufWriter::new(
        File::create(path).map_err(|e| format!("cannot create '{}': {e}", path.display()))?,
    );
    write_mastering_file(
        &mut writer,
        width,
        height,
        &static_meta,
        &[frame],
        MasteringCodec::Fpzip,
    )
    .map_err(|e| format!("cannot write mastering vector '{}': {e}", path.display()))?;
    Ok(())
}

fn transcode_mastering_file_deterministic(
    input_path: &Path,
    output_path: &Path,
    include_open_v2: bool,
    fidelity_contract: Option<&FidelityContract>,
    allow_vmaf_approximation: bool,
) -> Result<Vec<DeliveryFrame>, String> {
    let file = File::open(input_path).map_err(|e| {
        format!(
            "cannot open mastering input '{}': {e}",
            input_path.display()
        )
    })?;
    let mut stream = QdrvStreamReader::new(BufReader::new(file)).map_err(|e| {
        format!(
            "cannot read mastering input '{}': {e}",
            input_path.display()
        )
    })?;
    if stream.header().tier != TIER_MASTERING {
        return Err(format!(
            "input '{}' is not a mastering-tier file",
            input_path.display()
        ));
    }

    let source_static = stream.static_meta().clone();
    // FF-2: derive the *delivery* metadata schema version from the
    // include_open_v2 flag rather than blindly inheriting the source
    // mastering schema. Inheriting v2 without populating
    // `dynamic_meta.open_dynamic_v2` produces an inconsistent file that
    // `validate_compatibility` rejects ("schema v2 requires
    // open_dynamic_v2 payload"). The transcode pipeline below uses
    // `EncodeOptions::default()` which carries no v2 payload, so the
    // output must always be schema v1 — regardless of what schema version
    // the source mastering file carried.
    let mut static_meta = StaticMeta {
        qdrv_version: source_static.qdrv_version,
        metadata_schema_version: qdrv_meta::compatibility::METADATA_SCHEMA_V1,
        tier: Tier::Delivery,
        precision: qdrv_meta::Precision::Float32,
        colour_standard: source_static.colour_standard,
        colour_primaries: source_static.colour_primaries,
        transfer_function: "st2084_pq".to_string(),
        dynamic_metadata_standard: source_static.dynamic_metadata_standard,
        chroma_subsampling: source_static.chroma_subsampling,
        mastering_display: source_static.mastering_display,
        content_light_level: source_static.content_light_level,
        compatibility_tags: source_static.compatibility_tags,
    };
    if include_open_v2
        && !static_meta
            .compatibility_tags
            .iter()
            .any(|tag| tag == "open_dynamic_v2")
    {
        static_meta
            .compatibility_tags
            .push("open_dynamic_v2".to_string());
    }

    let mut delivery_frames = Vec::with_capacity(stream.frame_count() as usize);
    let mut frame_index = 0u64;
    while let Some(frame) = stream
        .next_frame()
        .map_err(|e| format!("cannot decode mastering frame {frame_index}: {e}"))?
    {
        let mastering_pixels = frame
            .pixels
            .as_mastering()
            .ok_or_else(|| format!("frame {frame_index} is not mastering-tier"))?;

        let encoded = transcode_frame_with_options(
            mastering_pixels,
            frame_index,
            static_meta.clone(),
            &EncodeOptions {
                deterministic: true,
                ..EncodeOptions::default()
            },
        )
        .map_err(|e| format!("transcode failed for frame {frame_index}: {e}"))?;

        if let Some(contract) = fidelity_contract {
            let ref_pixels = mastering_pixels
                .iter()
                .map(|p| {
                    qdrv_core::Pixel32::new_unchecked(
                        qdrv_core::pq::pq_oetf_f32((p.r / PQ_MAX_NITS).clamp(0.0, 1.0) as f32),
                        qdrv_core::pq::pq_oetf_f32((p.g / PQ_MAX_NITS).clamp(0.0, 1.0) as f32),
                        qdrv_core::pq::pq_oetf_f32((p.b / PQ_MAX_NITS).clamp(0.0, 1.0) as f32),
                    )
                })
                .collect::<Vec<_>>();
            let measured = measure_fidelity(
                &ref_pixels,
                &encoded.pixels,
                stream.header().width,
                stream.header().height,
                frame_index,
                contract,
                allow_vmaf_approximation,
            )?;
            let eval = contract.evaluate(&measured.measured);
            if !eval.passed {
                return Err(format!(
                    "fidelity contract failed at frame {frame_index}: {}",
                    eval.failures.join("; ")
                ));
            }
        }

        delivery_frames.push(DeliveryFrame {
            dynamic_meta: encoded.dynamic_meta,
            pixels: encoded.pixels,
        });
        frame_index += 1;
    }

    let av1 = Av1Config {
        speed: 6,
        quantizer: 0,
        lossless: true,
        threads: 1,
        chroma: ChromaSampling420::Cs444,
    };
    // FF-3: stage the delivery output through a `.part.<pid>` temp file
    // so a mid-write failure cannot leave a partial conformance candidate
    // masquerading as a complete one. Mirrors the `TempFileGuard` pattern
    // from `qdrv-tool/src/main.rs`.
    let tmp_path = {
        let ext = output_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}.part.{}", std::process::id()))
            .unwrap_or_else(|| format!("part.{}", std::process::id()));
        output_path.with_extension(ext)
    };
    struct ConformanceTempGuard {
        path: Option<PathBuf>,
    }
    impl Drop for ConformanceTempGuard {
        fn drop(&mut self) {
            if let Some(path) = self.path.take() {
                let _ = fs::remove_file(&path);
            }
        }
    }
    let mut tmp_guard = ConformanceTempGuard {
        path: Some(tmp_path.clone()),
    };
    {
        let mut writer = BufWriter::new(
            File::create(&tmp_path)
                .map_err(|e| format!("cannot create '{}': {e}", tmp_path.display()))?,
        );
        write_delivery_file(
            &mut writer,
            stream.header().width,
            stream.header().height,
            &static_meta,
            &delivery_frames,
            &av1,
        )
        .map_err(|e| {
            format!(
                "cannot write delivery output '{}': {e}",
                output_path.display()
            )
        })?;
        let file = writer
            .into_inner()
            .map_err(|e| format!("flush error for '{}': {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("fsync error for '{}': {e}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, output_path).map_err(|e| {
        format!(
            "atomic replace '{}' -> '{}' failed: {e}",
            tmp_path.display(),
            output_path.display()
        )
    })?;
    // Successful rename — suppress the guard's drop-time cleanup.
    tmp_guard.path = None;

    Ok(delivery_frames)
}

fn unix_ms_now() -> Result<u128, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock before unix epoch: {e}"))?
        .as_millis())
}

fn canonical_delivery_payload_sha256(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("cannot open '{}': {e}", path.display()))?;
    let mut stream = QdrvStreamReader::new(BufReader::new(file))
        .map_err(|e| format!("cannot parse '{}': {e}", path.display()))?;
    let mut canonical = Vec::new();
    while let Some(frame) = stream
        .next_frame()
        .map_err(|e| format!("cannot decode frame from '{}': {e}", path.display()))?
    {
        // Hash a canonical logical payload (metadata + pixel bits) instead of
        // raw container bytes so benign muxing/layout differences do not cause
        // false conformance failures.
        canonical.extend_from_slice(&frame.dynamic_meta.frame_index.to_le_bytes());
        let dynamic_json = serde_json::to_vec(&frame.dynamic_meta)
            .map_err(|e| format!("cannot serialize dynamic metadata for hash: {e}"))?;
        let len = u32::try_from(dynamic_json.len()).map_err(|_| {
            "dynamic metadata JSON length exceeds u32 in canonical hash".to_string()
        })?;
        canonical.extend_from_slice(&len.to_le_bytes());
        canonical.extend_from_slice(&dynamic_json);

        let pixels = frame
            .pixels
            .as_delivery()
            .ok_or_else(|| "canonical delivery hash requires delivery-tier frames".to_string())?;
        let px_len = u64::try_from(pixels.len())
            .map_err(|_| "pixel length does not fit u64 in canonical hash".to_string())?;
        canonical.extend_from_slice(&px_len.to_le_bytes());
        for p in pixels {
            canonical.extend_from_slice(&p.r.to_bits().to_le_bytes());
            canonical.extend_from_slice(&p.g.to_bits().to_le_bytes());
            canonical.extend_from_slice(&p.b.to_bits().to_le_bytes());
        }
    }
    Ok(sha256_hex(&canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rel_rejects_parent_traversal() {
        let root = Path::new("/tmp/manifest-root");
        let err = resolve_rel(root, "../etc/passwd").unwrap_err();
        assert!(err.contains("escapes"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_rel_rejects_absolute_path() {
        let root = Path::new("/tmp/manifest-root");
        #[cfg(unix)]
        let err = resolve_rel(root, "/etc/passwd").unwrap_err();
        #[cfg(windows)]
        let err = resolve_rel(root, "C:\\Windows\\System32\\cmd.exe").unwrap_err();
        assert!(err.contains("must be relative"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_rel_accepts_simple_relative_path() {
        let root = Path::new("/tmp/manifest-root");
        let resolved = resolve_rel(root, "subdir/file.json").unwrap();
        assert!(resolved.starts_with(root));
    }

    #[test]
    fn validate_vector_id_rejects_path_separators_and_dots() {
        assert!(validate_vector_id("../escape").is_err());
        assert!(validate_vector_id("dir/sub").is_err());
        assert!(validate_vector_id("a\\b").is_err());
        assert!(validate_vector_id("").is_err());
        assert!(validate_vector_id(&"x".repeat(129)).is_err());
        // Note: '.' is intentionally disallowed (not in the allow-list) so a
        // hostile manifest cannot author dotfile candidate names.
        assert!(validate_vector_id("hidden.qdrv").is_err());
    }

    #[test]
    fn validate_vector_id_accepts_typical_names() {
        assert!(validate_vector_id("open-vector-000").is_ok());
        assert!(validate_vector_id("my_vector_42").is_ok());
        assert!(validate_vector_id("X").is_ok());
    }

    #[test]
    fn open_vectors_manifest_roundtrip() {
        let manifest = ConformanceCorpusManifest {
            schema_version: CONFORMANCE_SCHEMA_VERSION,
            corpus_name: "test".to_string(),
            generated_unix_ms: 123,
            vectors: vec![ConformanceVector {
                id: "v0".to_string(),
                input_mastering_path: "in.qdrv64".to_string(),
                golden_delivery_path: "gold.qdrv32".to_string(),
                expected_delivery_sha256: "abc".to_string(),
                fidelity_contract_path: None,
                golden_dynamic_meta_json_path: None,
                golden_dynamic_meta_manifest_path: None,
            }],
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let recovered: ConformanceCorpusManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.schema_version, CONFORMANCE_SCHEMA_VERSION);
        assert_eq!(recovered.vectors.len(), 1);
    }

    /// P6-1 regression: `generate_open_vectors` must produce a corpus that
    /// `run_conformance` accepts end-to-end. Without this test the prior
    /// audit cycle missed a contract violation between the generator and
    /// the P3-3 reader check (the generator stored the vector index as
    /// `frame_index`, which the reader then rejected as a sequencing
    /// mismatch for any vector beyond the first).
    ///
    /// Generates 2 vectors in a temp dir, runs the full conformance check
    /// against the same dir, and asserts every vector passes.
    #[test]
    fn generate_and_run_open_vectors_end_to_end() {
        let root = {
            let mut p = std::env::temp_dir();
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            p.push(format!(
                "qdrv-conformance-e2e-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&p).expect("create temp dir");
            p
        };
        let corpus_dir = root.join("corpus");
        let run_dir = root.join("run");

        // Generation: this is the path that P6-1 broke. The previous
        // single-vector check passed accidentally because the bug only
        // shows up for `idx >= 1`.
        // The 32×32 corpus is below the libvmaf 33-pixel minimum so the
        // deterministic VMAF-HDR surrogate is the only available scorer.
        // Pass `allow_vmaf_approximation = true` to acknowledge that the
        // approximation is acceptable for this synthetic test fixture;
        // production callers should leave this `false` unless they have
        // explicitly opted in (see `QDRV_VMAF_HDR_ALLOW_APPROX`). Audit
        // MEDIUM `AUDIT_REPORT_28-05-2026_2053.md`.
        let manifest_path = generate_open_vectors(
            &corpus_dir,
            "qdrv-pass6-regression",
            &OpenVectorsConfig {
                vector_count: 2,
                width: 32,
                height: 32,
                allow_vmaf_approximation: true,
            },
            b"qdrv-pass6-key",
            "qdrv-pass6-signer",
        )
        .expect("generate_open_vectors must succeed for vector_count > 1");

        // Run: re-transcodes each vector and checks the canonical hash +
        // metadata signature + fidelity contract. All vectors must pass.
        let summary = run_conformance(&manifest_path, &run_dir, b"qdrv-pass6-key", true)
            .expect("run_conformance must succeed against a freshly-generated corpus");

        assert_eq!(summary.total_vectors, 2);
        assert_eq!(
            summary.passed_vectors, 2,
            "expected all vectors to pass, summary: {:#?}",
            summary
        );
        assert!(summary.all_passed);
        for result in &summary.results {
            assert!(
                result.passed,
                "vector {} failed: {:?}",
                result.id, result.failures
            );
            assert!(result.hash_match, "hash mismatch for vector {}", result.id);
            assert!(
                result.signature_verified,
                "signature verification failed for vector {}",
                result.id
            );
        }

        let _ = fs::remove_dir_all(&root);
    }
}
