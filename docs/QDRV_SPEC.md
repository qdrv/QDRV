# QDRV Specification

Author: Michael Lauzon <qdrv2026@gmail.com>

## Quantum Dynamic Range Video

**Version:** 0.1.0 (Working Draft)  
**Status:** Implementation-aligned profile  
**Licence:** GNU General Public Licence v2.0 (GPLv2)

## 1. Scope

This document defines the currently implemented QDRV profile and conformance expectations.

QDRV (Quantum Dynamic Range Video) is a two-tier floating-point dynamic-range video format, intended as a successor to integer HDR (HDR10, HDR10+) and to proprietary Dolby Vision packaging:

- mastering tier (`.qdrv64`): Float64 linear-light RGB
- delivery tier (`.qdrv32`): Float32 SMPTE ST 2084 PQ RGB at API boundary, AV1 payload in container frames

## 2. Normative Language

- **MUST** indicates required behaviour.
- **SHOULD** indicates recommended behaviour.
- **MAY** indicates optional behaviour.

## 3. Standards Foundation

QDRV implementation is grounded in public standards:

- ITU-R Rec. 2100 (BT.2100)
- ITU-R Rec. 2020 (BT.2020)
- SMPTE ST 2084
- SMPTE ST 2094 framework
- SMPTE ST 2086
- ITU-R BT.2408

## 4. Container and Tier Architecture

### 4.1 Tiers

- `.qdrv64`: Float64 mastering, lossless mastering codec payloads (`fpzip` default, optional `zfp`)
- `.qdrv32`: Float32 delivery, AV1 codec payloads

### 4.2 Container versions and header

QDRV supports container versions **v1** and **v2**.

- Reader implementations MUST accept both v1 and v2.
- Writer implementations SHOULD default to v2 for new outputs.
- Writer implementations MAY emit v1 when explicitly requested for compatibility.
- Unknown/deprecated versions MUST fail with unsupported-version errors.
- Future versions MUST fail with future-version errors.

Both v1 and v2 use the same fixed 28-byte little-endian header layout:

- magic (`QDRV`)
- format version
- tier byte
- codec byte
- width and height
- frame count
- reserved flags (MUST be zero)
- static metadata length

### 4.3 Container/metadata coherence

- Container v1 MUST carry metadata schema v1 (`metadata_schema_version = 1`).
- Container v2 MAY carry metadata schema v1 or v2.
- Static and dynamic metadata schema versions MUST match.

### 4.4 Frame block layout

Each frame carries dynamic metadata followed by pixel data; the exact
shape of the pixel-data section depends on the container's codec byte:

**Codec byte `1` (production AV1 / fpzip / ZFP):**

1. dynamic metadata length (`u32 LE`)
2. dynamic metadata JSON (UTF-8)
3. pixel payload length (`u32 LE`)
4. pixel payload bytes (AV1 still picture for delivery; fpzip or ZFP
   blob with leading per-blob codec identifier byte for mastering)

**Codec byte `0` (raw uncompressed — testing/diagnostic only):**

1. dynamic metadata length (`u32 LE`)
2. dynamic metadata JSON (UTF-8)
3. raw pixel bytes — **no length prefix**; the byte count is implicit
   from `width × height × channels × bytes_per_channel` (delivery:
   `× 3 × 4` = 12 bytes/pixel, Float32 LE RGB; mastering: `× 3 × 8`
   = 24 bytes/pixel, Float64 LE RGB)

Raw codec mode is intended for tests and diagnostics, not production
interchange.

## 5. Metadata Compatibility and v2

### 5.1 Base metadata

`DynamicMeta` includes scene luminance statistics, tone curve, display hint, optional v2 structures, optional inverse mapping hint, and creator intent lock state.

### 5.2 Schema contracts

Compatibility rules in current implementation:

- static and dynamic metadata schema versions MUST match
- schema v2 (`METADATA_SCHEMA_V2`) MUST carry `open_dynamic_v2`
- schema v1 MUST NOT carry `open_dynamic_v2`
- mastering-tier (`tier == Mastering`) streams MUST NOT carry delivery-only
  adaptation policy fields. The gated fields are:
  - `DynamicMeta.inverse_tone_mapping_hint`
  - `OpenDynamicMetadataV2.adaptation_layer`
  - `OpenDynamicMetadataV2.ambient_policy`
  - `OpenDynamicMetadataV2.gaming_profile`
  - `OpenDynamicMetadataV2.inverse_tone_mapping_hint`

  The companion creative-intent fields (`scene_constraints`,
  `object_constraints`, `temporal`, `local_tone_map_grid`) ARE permitted on
  mastering-tier streams: they describe authorial intent that survives the
  mastering-to-delivery transcode and is reused by the delivery tone
  mapper.

### 5.3 Open Dynamic Metadata v2 structures

Current v2 payload supports:

- `scene_constraints`
- `object_constraints`
- `temporal`
- `local_tone_map_grid`
- `adaptation_layer`
- `ambient_policy`
- `gaming_profile`
- `inverse_tone_mapping_hint`

## 6. HDR10+ Profile Export Contract

`qdrv hdr10plus` exports a machine-readable JSON object with:

- `mode`: one of `basic`, `advanced`, `adaptive`, `gaming`
- `compatibility`: explicit certification status and missing capabilities
- `entries`: per-frame profile payloads tagged by profile mode

### 6.1 Profile modes

- `basic`: ST 2094-40-style 10-bit fields
- `advanced`: 10-bit compatibility fields plus 16-bit extended fields
- `adaptive`: advanced base plus ambient-policy-derived adaptation fields
- `gaming`: advanced base plus low-latency temporal/gaming-derived fields

### 6.2 Certification boundary

Open HDR10+ profile exports in this repository are **compatibility outputs**.

- `certification_status` is currently `not_certified`
- `certified_output_generated` is currently `false`
- certification-capable output requires external proprietary workflows and credentials not shipped here

## 7. Interoperability Export Profile

`qdrv export-interop` outputs:

- HDR10 raw RGB10LE payload
- HDR10+ profile JSON (`mode=basic`) with compatibility metadata
- open DV-compatible sidecar JSON
- combined interop loss report JSON
- DV adapter status report JSON

Interop reports MUST enumerate dropped fields, approximated fields, and unsupported features.

## 8. CLI Conformance Profile (Current)

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

Key implemented flags:

- `write-test`: `--width`, `--height`, `--frames`, `--mastering`, `--quantizer`, `--speed`, `--mastering-codec`, `--container-version {v1|v2}`
- `convert`: `--sdr`, `--hdr10`, `--deterministic`, `--creator-intent-lock`, `--metadata-v2`, `--ambient-lux`, `--display-model`, `--frame-time-ms`, `--fidelity-contract`, `--interop-report`, `--dv-sidecar`, `--container-version {v1|v2}`
- `hdr10plus`: `--mode {basic|advanced|adaptive|gaming}`, legacy `--advanced`
- `inspect`: `--meta`, `--frames`, `--render-frame-time-ms`, `--render-target-max-nits`
- `pq`: `--nits <NITS>` (nits → PQ), `--pq <PQ>` (PQ → nits); mutually exclusive
- `mux`: `--frame-rate`, `--quantizer`, `--speed`, `--keyframe-interval`
- `export-interop`: `--dv-tool-cmd`
- `conformance-generate-open`: `--key`, `--key-file`, `--allow-public-default-key`, `--signer`, `--corpus-name`, `--vectors`, `--width`, `--height`

`mux` re-encodes a delivery-tier `.qdrv32` stream through the AV1 temporal/GOP encoder and writes a minimal ISOBMFF (`.mp4`) container with one video track. Output carries an HDR `colr` `nclx` box advertising BT.2020 primaries, SMPTE ST 2084 transfer, and BT.2020 NCL matrix coefficients. Mastering-tier inputs are rejected.

## 9. Current Limitations and External Dependencies

- Open code does not generate certified Dolby Vision bitstreams.
- Open HDR10+ profile exports are not certified outputs.
- ZFP mastering compression requires optional feature build (`--features zfp`).
- External VMAF-HDR backends are optional; deterministic fallback remains available.
- Raw codec mode is intended for tests, not production interchange.
- `qdrv convert` currently accepts mastering-tier input only.

## 10. Validation Commands

The conformance gate is formatting parity, lint policy under
`[workspace.lints]` in the root `Cargo.toml`, and the full workspace test
suite under both default and `qdrv-codec/zfp` feature configurations.
All commands MUST finish with zero warnings and zero failed tests.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features qdrv-codec/zfp -- -D warnings
cargo test --workspace
cargo test --workspace --features qdrv-codec/zfp
```
