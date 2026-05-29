# QDRV Technical Reference

Author: Michael Lauzon <qdrv2026@gmail.com>

## Implementation Details for v0.1.0

**Document version:** 0.1.0  
**Container version:** default write v2; optional compatibility write v1; read support v1 and v2  
**Licence:** GNU General Public Licence v2.0 (GPLv2)

This technical reference tracks implemented behaviour only.

## 1. Current Implementation Capabilities

- Two-tier floating-point dynamic-range video format (`.qdrv64` mastering, `.qdrv32` delivery), positioned as a successor to integer HDR (HDR10, HDR10+) and Dolby Vision
- Full CLI workflow for generation, conversion, inspection, metadata signing, conformance, and interoperability export
- Open Dynamic Metadata v2 data structures and decode-policy behaviour
- Deterministic conversion and render policy support
- Fidelity contract enforcement during conversion and conformance runs
- Explicit interop loss reporting and proprietary-adapter boundary modelling
- Streaming reader path plus decoder and mux robustness improvements

## 2. Command Surface (`qdrv-tool/src/main.rs`)

Implemented commands:

- `info`
- `pq`
- `meta-static`
- `meta-dynamic`
- `meta-dynamic-v2`
- `write-test`
- `convert`
- `hdr10plus`
- `inspect`
- `mux`
- `export-interop`
- `manifest-sign`
- `manifest-verify`
- `conformance-generate-open`
- `conformance-run`

Key flags currently implemented:

- `write-test`: `--width`, `--height`, `--frames`, `--mastering`, `--quantizer`, `--speed`, `--mastering-codec`, `--container-version {v1|v2}`
- `convert`: `--sdr`, `--hdr10`, `--deterministic`, `--creator-intent-lock`, `--metadata-v2`, `--ambient-lux`, `--display-model`, `--frame-time-ms`, `--fidelity-contract`, `--interop-report`, `--dv-sidecar`, `--container-version {v1|v2}`
- `inspect`: `--meta`, `--frames`, `--render-frame-time-ms`, `--render-target-max-nits`
- `hdr10plus`: `--mode {basic|advanced|adaptive|gaming}`, legacy `--advanced`
- `pq`: `--nits <NITS>` (nits → PQ), `--pq <PQ>` (PQ → nits); mutually exclusive
- `mux`: `--frame-rate`, `--quantizer`, `--speed`, `--keyframe-interval`
- `export-interop`: `--dv-tool-cmd`
- `conformance-generate-open`: `--key`, `--key-file`, `--allow-public-default-key`, `--signer`, `--corpus-name`, `--vectors`, `--width`, `--height`

The `mux` command reads a delivery-tier `.qdrv32` file, sends each decoded `Pixel32` frame through `qdrv-codec::TemporalEncoder` (rav1e in GOP mode, low-latency, 12-bit 4:4:4, BT.2020 NCL), collects the resulting AV1 packets in presentation order, and hands them to `qdrv-mux::write_mp4`. The output is a single-track ISOBMFF (`.mp4`) container with an HDR `colr nclx` box (BT.2020 primaries, SMPTE ST 2084 transfer, BT.2020 NCL matrix). Mastering-tier inputs are rejected at the top of `cmd_mux` before any rav1e initialisation, and the muxer automatically promotes `stco` → `co64` and `mdat` → `largesize` headers when offsets or payloads exceed the 32-bit ISOBMFF limits.

## 3. HDR10+ Profile Export Implementation

### 3.1 Export modes and mapping

`qdrv-meta/src/hdr10plus.rs` currently provides:

- `to_hdr10plus_entry` (basic)
- `to_hdr10plus_advanced_entry` (advanced)
- `to_hdr10plus_adaptive_entry` (adaptive-compatible)
- `to_hdr10plus_gaming_entry` (gaming-compatible)

The profile exporter builds a mode-tagged payload:

- `Hdr10PlusProfileExport`
- `Hdr10PlusProfiledEntry`
- `Hdr10PlusCompatibilityReport`

### 3.2 Machine-readable profile metadata

Each export object includes:

- `mode` (`basic`, `advanced`, `adaptive`, `gaming`)
- `compatibility.schema`
- `compatibility.certification_status`
- `compatibility.certified_output_generated`
- `compatibility.requires_vendor_certification`
- `compatibility.missing_capabilities`

### 3.3 Certification boundary

Open HDR10+ profile exports are explicitly marked as not certified in current code paths:

- `certification_status = not_certified`
- `certified_output_generated = false`

No proprietary certification workflow is implemented in this repository.

## 4. Interoperability Export and Proprietary Boundary

`qdrv export-interop` (`qdrv-tool/src/interop_export.rs`) emits:

- HDR10 raw (`interop.hdr10.rgb10le.raw`)
- HDR10+ profile JSON (`interop.hdr10plus.json`, mode `basic`)
- DV-compatible sidecar (`interop.dv-compatible.json`)
- combined loss report (`interop.loss-report.json`)
- adapter report (`interop.dv-adapter-report.json`)

The combined loss report includes:

- `hdr10`
- `hdr10plus`
- `hdr10plus_compatibility`
- `dolby_vision_compatible`
- `dv_adapter_status`

Certified Dolby Vision packaging is modelled as an external proprietary adapter boundary (`--dv-tool-cmd` with `{sidecar}`, `{rpu}`, `{report}` placeholders).

## 5. Open Dynamic Metadata v2 Implementation

### 5.1 Data model

`qdrv-meta/src/open_dynamic_v2.rs` includes:

- `LocalToneMapGrid`
- `DisplayAdaptationLayer`
- `AmbientAdaptivePolicy`
- `GamingProfile`
- `TemporalConstraint`
- `InverseToneMappingHint`

Validation enforces finite values, monotonic structures where required, and range constraints.

### 5.2 Runtime behaviour

`qdrv-decode/src/tone_map.rs` can apply:

- local grid gain/offset sampling
- display adaptation by model class
- ambient lux boost policy
- gaming-profile temporal control and anti-pumping
- deterministic render quantisation when requested

Creator intent lock can suppress non-authorial adaptation.

## 6. Deterministic Mode and Fidelity Contracts

`qdrv convert --deterministic` enables deterministic choices in conversion paths.

`FidelityContract` supports thresholds for:

- `psnr_db_min`
- `ssim_min`
- `delta_e_max`
- `vmaf_hdr_min`

Contract failures are explicit and abort the operation.

VMAF-HDR backend order in `qdrv-tool/src/fidelity_eval.rs`:

1. custom template command (`QDRV_VMAF_HDR_CMD`)
2. ffmpeg/libvmaf detection path
3. deterministic approximation fallback

## 7. Streaming Read, Mux, and Buffer Reuse

### 7.1 Streaming reader

`qdrv-io/src/reader.rs` provides `QdrvStreamReader` for frame-by-frame decode without mandatory full-file materialisation.

### 7.2 Reader/writer hardening

Current protections include:

- metadata size caps
- frame count and frame area bounds
- compressed payload budget limits
- checked arithmetic and allocation error paths
- strict static/dynamic metadata compatibility checks

### 7.3 Decoder and buffer reuse

`qdrv-codec/src/av1.rs` includes reusable decoder state and scratch buffers.

### 7.4 MP4 mux robustness

`qdrv-mux/src/lib.rs` includes:

- safe size arithmetic for box construction
- automatic `stco` vs `co64` chunk-offset selection
- `mdat` `largesize` handling for payloads exceeding 32-bit box size limits
- HDR colour signalling via a `colr` `nclx` box (BT.2020 primaries,
  SMPTE ST 2084 transfer, BT.2020 NCL matrix, full pixel range) so MP4-
  level players that read HDR characteristics from the container — rather
  than parsing the AV1 OBU — see the correct interpretation

## 8. Limitations and External Dependencies

- Open HDR10+ profile exports are compatibility-oriented and not certified
- Certified Dolby Vision bitstream packaging is external and proprietary
- `zfp` mastering compression is optional and feature-gated
- VMAF-HDR high-fidelity scoring depends on external tooling; deterministic fallback remains available
- Raw codec mode is test-focused
- `qdrv convert` currently requires mastering-tier input

## 9. Verification Commands

The verification gate is formatting parity, the workspace lint policy
under `[workspace.lints]` in the root `Cargo.toml`, and the full
workspace test suite under both default and `qdrv-codec/zfp` feature
configurations. All commands MUST finish with zero warnings and zero
failed tests.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features qdrv-codec/zfp -- -D warnings
cargo test --workspace
cargo test --workspace --features qdrv-codec/zfp
```

## 10. Documentation Policy

Project documentation uses Canadian English spelling and punctuation (for example: colour, behaviour, licence, artefact, serialisation) while keeping source-code identifiers and CLI flags exact.
