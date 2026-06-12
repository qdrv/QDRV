# QDRV Specification

Author: Michael Lauzon <qdrv2026@gmail.com>

## Quantum Dynamic Range Video

**Version:** 0.1.0 (First Public Release)  
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
- `spherical_projection` (optional; 360°/immersive projection — see §5.4)

### 5.4 Spatial metadata for 360°/immersive video

QDRV carries per-region tone-mapping intent for spherical (360°/immersive)
content using angular coordinates, alongside the rectilinear `object_meta`
regions used for flat content.

**Projection.** `OpenDynamicMetadataV2.spherical_projection` is an optional,
stream-level field naming the projection by which the flat delivery raster
maps onto the unit sphere. It is constant for the stream. Defined values:

- `equirectangular` — longitude maps linearly to the horizontal axis,
  latitude linearly to the vertical axis.
- `cubemap` — standard six-face cubemap layout.
- `equi_angular_cubemap` — cubemap with equal-angle sampling (EAC).

When `spherical_projection` is absent, the stream is rectilinear and only the
flat `object_meta` regions apply.

**Regions.** Per-frame spherical regions live in
`object_meta.spherical_regions`. Each `SphericalRegion` carries the same
`tone_map_curve` and `priority` fields as a rectilinear `ObjectRegion`, plus
an angular location in radians:

- `centre_azimuth` — longitude of the region centre, in `[-π, π]`.
- `centre_elevation` — latitude of the region centre, in `[-π/2, π/2]`.
- `angular_width` — full longitude extent, in `(0, 2π]`.
- `angular_height` — full latitude extent, in `(0, π]`.

Region IDs MUST be unique within a frame's spherical region set. Priority
resolves overlaps exactly as for rectilinear regions: the highest-priority
region containing a coordinate wins.

**Coordinate convention (equirectangular).** A normalised raster coordinate
`(nx, ny)` in `[0, 1]` maps to `azimuth = (nx − 0.5)·2π` and
`elevation = (0.5 − ny)·π`. Image row 0 is the north pole (`+π/2`); the
bottom row is the south pole (`−π/2`); `nx = 0.5` is frame-forward
(`azimuth = 0`).

**Pole and seam rules.**

- Latitude is bounded: a region's latitude extent MUST stay within the poles
  (`centre_elevation ± angular_height/2` within `[-π/2, π/2]`).
- Longitude is cyclic: it wraps across the antimeridian at `±π`. Region
  containment in longitude uses the shortest signed angular distance to
  `centre_azimuth`, so a region whose extent crosses the seam matches
  coordinates on both sides.

**Decoder support.** All three projections are implemented in the delivery
tone mapper. The cubemap layouts use the canonical QDRV 3×2 face grid (3
columns × 2 rows):

```text
  row 0:  +X  +Y  +Z
  row 1:  -X  -Y  -Z
```

Face-local coordinates run in `[-1, 1]` per face; the equi-angular cubemap
applies the EAC warp (`tan` of an angle linear in the pixel) so the pixel grid
samples equal angles. Face orientation follows the standard cubemap
convention, with azimuth 0 at `+Z` (frame-forward) and elevation `+π/2` at
`+Y` (up), matching the equirectangular convention above. Content authored for
cubemap or EAC delivery MUST use this layout for region coordinates to resolve
correctly.

**Compatibility.** These fields are additive within metadata schema v2 and
default to empty/absent, so existing v2 streams parse unchanged. They are
structural/authorial metadata and are permitted on both mastering and
delivery tiers.

### 5.5 Rectilinear object motion

Rectilinear `object_meta.regions[]` entries MAY carry a `motion` field. When
absent, the region is same-frame metadata and applies only when
`object_meta.frame_index` equals the dynamic metadata frame being rendered.
When present, motion is measured from `object_meta.frame_index`, which acts as
the authored keyframe.

The motion schema is internally tagged by `kind`:

```json
{
  "kind": "translate",
  "dx_per_frame": 0.01,
  "dy_per_frame": 0.0,
  "frame_count": 12
}
```

Defined values:

- `static` — keeps the bounding box fixed for `frame_count` frames.
- `translate` — shifts the bounding box by `dx_per_frame` and `dy_per_frame`
  normalised-coordinate units per frame for `frame_count` frames.
- `piecewise_linear` — interpolates explicit normalised offsets between
  `keyframes`.

Piecewise-linear motion is encoded as:

```json
{
  "kind": "piecewise_linear",
  "keyframes": [
    { "frame_delta": 0, "dx": 0.0, "dy": 0.0 },
    { "frame_delta": 12, "dx": 0.10, "dy": 0.00 },
    { "frame_delta": 24, "dx": 0.05, "dy": 0.08 }
  ]
}
```

`frame_count` MUST be greater than zero and includes the authored keyframe.
Translation deltas MUST be finite. Piecewise-linear keyframes MUST contain two
to 64 entries, MUST start at `frame_delta = 0` with `dx = 0` and `dy = 0`, and
MUST be strictly increasing by `frame_delta`. Each keyframe offset MUST be
finite. Validators MUST reject a motion descriptor whose active bounding boxes
leave the `[0, 1]` unit square; for linear translation and piecewise-linear
segments, validating the segment endpoints proves every intermediate frame is
also contained. Spherical regions do not currently carry motion descriptors.

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
- `object-motion`
- `write-test`
- `convert`
- `aces-export`
- `hdr10plus`
- `inspect`
- `mux`
- `still`
- `probe-stream`
- `export-interop`
- `manifest-sign`
- `manifest-verify`
- `conformance-generate-open`
- `conformance-run`

Key implemented flags:

- `write-test`: `--width`, `--height`, `--frames`, `--mastering`, `--quantizer`, `--speed`, `--mastering-codec`, `--container-version {v1|v2}`
- `convert`: `--sdr`, `--hdr10`, `--quantizer`, `--speed`, `--deterministic`, `--creator-intent-lock`, `--metadata-v2`, `--ambient-lux`, `--display-model`, `--frame-time-ms`, `--fidelity-contract`, `--interop-report`, `--dv-sidecar`, `--container-version {v1|v2}`
- `aces-export`: `--target {aces2065-1|rec709-100nit|rec2020-1000nit|rec2020-4000nit}`, `--reference-white-nits`, `--prefix`, `--start-number`
- `object-motion`: `--region-id`, `--kind {static|translate|piecewise-linear}`, `--frame-count`, `--dx-per-frame`, `--dy-per-frame`, `--to-x`, `--to-y`, repeated `--keyframe FRAME_DELTA:DX:DY`, `--overwrite`
- `hdr10plus`: `--mode {basic|advanced|adaptive|gaming}`, legacy `--advanced`
- `inspect`: `--meta`, `--frames`, `--render-frame-time-ms`, `--render-target-max-nits`
- `pq`: `--nits <NITS>` (nits → PQ), `--pq <PQ>` (PQ → nits); mutually exclusive
- `mux`: `--frame-rate`, `--quantizer`, `--speed`, `--keyframe-interval`, `--format {mp4|fmp4|cmaf|ivf|obu}`
- `still`: `--frame-index`, `--quantizer`, `--speed`, `--deterministic`
- `probe-stream`: positional input only (`.mp4`/fragmented/CMAF, `.ivf`, or raw `.obu`)
- `export-interop`: `--dv-tool-cmd`
- `manifest-sign`: `--key`, `--key-file`, `--signer`
- `manifest-verify`: `--key`, `--key-file`
- `conformance-generate-open`: `--key`, `--key-file`, `--allow-public-default-key`, `--signer`, `--corpus-name`, `--vectors`, `--width`, `--height`
- `conformance-run`: `--key`, `--key-file`

`mux` re-encodes a delivery-tier `.qdrv32` stream through the AV1 temporal/GOP encoder and writes the result in the format selected by `--format`: a progressive ISOBMFF (`mp4`, the default), a fragmented ISOBMFF / CMAF stream segmented at keyframes for adaptive streaming (`fmp4`/`cmaf`), or a bare AV1 elementary stream (`ivf`/`obu`) for codec tooling. The ISOBMFF outputs carry an HDR `colr` `nclx` box advertising BT.2020 primaries, SMPTE ST 2084 transfer, and BT.2020 NCL matrix coefficients. Each frame's dynamic metadata is embedded in the AV1 bitstream as an ITU-T T.35 metadata OBU, so it travels with the stream into every container target. Mastering-tier inputs are rejected.

`still` exports a single frame from a delivery-tier `.qdrv32` or mastering-tier `.qdrv64` stream as an AVIF still image. Delivery inputs are encoded directly from their selected `Pixel32` frame; mastering inputs are transcoded through the existing mastering-to-delivery path before AV1 still-picture encode. The AVIF file uses `avif` as the major brand and includes `mif1`, `miaf`, and `avis` compatible brands. The primary item is an `av01` image item with HDR `colr` `nclx` signalling. QDRV metadata is preserved as an `application/qdrv+json` MIME metadata item containing the source container version, source tier, selected frame index, static metadata, and dynamic metadata; the metadata item is linked to the primary image from the AVIF `meta` box.

`probe-stream` is the read-side counterpart: it demuxes an exported stream (progressive, fragmented, or CMAF MP4; IVF; or raw OBU), extracts the embedded ITU-T T.35 metadata OBUs, and prints a per-frame dynamic-metadata summary.

`aces-export` reads a delivery-tier `.qdrv32` stream or a mastering-tier `.qdrv64` stream and writes one OpenEXR file per frame. The `aces2065-1` target writes scene-linear ACES AP0 interchange RGB without applying a display rendering transform. The `rec709-100nit`, `rec2020-1000nit`, and `rec2020-4000nit` targets apply the ACES RRT/ODT output paths implemented in `qdrv-core::aces`. Output filenames are `<prefix>_<frame-number:06>.exr`, starting at `--start-number`; writes go through a `.part.<pid>` temporary path before atomic replacement. `--reference-white-nits` defines the absolute QDRV luminance mapped to ACES scene-linear `1.0` and must be positive and finite.

The exported streams use only standard AV1 and ISOBMFF / CMAF structures, with no QDRV-proprietary container framing, and the dynamic metadata is carried in-bitstream as ITU-T T.35 AV1 metadata OBUs rather than in container-specific boxes. The outputs therefore interoperate with the standard AV1 toolchain: the progressive MP4, fragmented MP4, and CMAF outputs parse under GPAC MP4Box and decode under ffmpeg and dav1d; the fragmented MP4 / CMAF output packages into MPEG-DASH and HLS under Shaka Packager; and the IVF and raw-OBU elementary streams decode under dav1d. This is a design property — QDRV emits conformant AV1-in-ISOBMFF — not a certification claim, and these acceptance checks are run manually rather than as part of the gated test suite.

## 9. Current Limitations and External Dependencies

- Open code does not generate certified Dolby Vision bitstreams.
- Open HDR10+ profile exports are not certified outputs.
- ZFP mastering compression requires optional feature build (`--features zfp`).
- External VMAF-HDR backends are optional; deterministic fallback remains available.
- Raw codec mode is intended for tests, not production interchange.
- `qdrv convert` currently accepts mastering-tier input only.
- In-browser playback (`qdrv-decode-wasm` plus the `examples/web/` page) is an experimental, browser-dependent demonstration; it is not part of the conformance gates.

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
