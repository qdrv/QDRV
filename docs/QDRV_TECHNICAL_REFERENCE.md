# QDRV Technical Reference

Author: Michael Lauzon <qdrv2026@gmail.com>

## Implementation Details for v0.1.0

**Document version:** 0.1.0  
**Container version:** default write v2; optional compatibility write v1; read support v1 and v2  
**Licence:** GNU General Public Licence v2.0 or later (GPLv2+)

This technical reference tracks implemented behaviour only.

## 1. Current Implementation Capabilities

- Two-tier floating-point dynamic-range video format (`.qdrv64` mastering, `.qdrv32` delivery), positioned as a successor to integer HDR (HDR10, HDR10+) and Dolby Vision
- Full CLI workflow for generation, conversion, inspection, metadata signing, conformance, and interoperability export
- Open Dynamic Metadata v2 data structures and decode-policy behaviour
- Object-based and 360°/immersive (spherical) per-region tone mapping
- Deterministic conversion and render policy support
- Fidelity contract enforcement during conversion and conformance runs
- Explicit interop loss reporting and proprietary-adapter boundary modelling
- Streaming reader path plus decoder and mux robustness improvements
- In-browser WebAssembly decode build (`qdrv-decode-wasm`) with a WebGPU-first example page

## 2. Command Surface (`qdrv-tool/src/main.rs`)

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

Key flags currently implemented:

- `write-test`: `--width`, `--height`, `--frames`, `--mastering`, `--quantizer`, `--speed`, `--mastering-codec`, `--container-version {v1|v2}`
- `convert`: `--sdr`, `--hdr10`, `--quantizer`, `--speed`, `--deterministic`, `--creator-intent-lock`, `--metadata-v2`, `--ambient-lux`, `--display-model`, `--frame-time-ms`, `--fidelity-contract`, `--interop-report`, `--dv-sidecar`, `--container-version {v1|v2}`
- `aces-export`: `--target {aces2065-1|rec709-100nit|rec2020-1000nit|rec2020-4000nit}`, `--reference-white-nits`, `--prefix`, `--start-number`
- `inspect`: `--meta`, `--frames`, `--render-frame-time-ms`, `--render-target-max-nits`
- `object-motion`: `--region-id`, `--kind {static|translate|piecewise-linear}`, `--frame-count`, `--dx-per-frame`, `--dy-per-frame`, `--to-x`, `--to-y`, repeated `--keyframe FRAME_DELTA:DX:DY`, `--overwrite`
- `hdr10plus`: `--mode {basic|advanced|adaptive|gaming}`, legacy `--advanced`
- `pq`: `--nits <NITS>` (nits → PQ), `--pq <PQ>` (PQ → nits); mutually exclusive
- `mux`: `--frame-rate`, `--quantizer`, `--speed`, `--keyframe-interval`, `--format {mp4|fmp4|cmaf|ivf|obu}`
- `still`: `--frame-index`, `--quantizer`, `--speed`, `--deterministic`
- `probe-stream`: positional input only (`.mp4`/fragmented/CMAF, `.ivf`, or raw `.obu`)
- `export-interop`: `--dv-tool-cmd`
- `manifest-sign`: `--key`, `--key-file`, `--signer`
- `manifest-verify`: `--key`, `--key-file`
- `conformance-generate-open`: `--key`, `--key-file`, `--allow-public-default-key`, `--signer`, `--corpus-name`, `--vectors`, `--width`, `--height`
- `conformance-run`: `--key`, `--key-file`

The `mux` command reads a delivery-tier `.qdrv32` file, sends each decoded `Pixel32` frame through `qdrv-codec::TemporalEncoder` (rav1e in GOP mode, low-latency, 12-bit 4:4:4, BT.2020 NCL), collects the resulting AV1 packets in presentation order, embeds each frame's dynamic metadata into its temporal unit as an ITU-T T.35 metadata OBU (`qdrv-codec::embed_qdrv_metadata`), and writes the packets in the format chosen by `--format`. The default `mp4` path uses `qdrv-mux::write_mp4` (single-track progressive ISOBMFF with an HDR `colr nclx` box: BT.2020 primaries, SMPTE ST 2084 transfer, BT.2020 NCL matrix); `fmp4`/`cmaf` use `qdrv-mux::write_fmp4` / `write_cmaf` to emit an initialisation segment plus keyframe-aligned media segments (`moof`/`traf`/`trun`); and `ivf`/`obu` use `qdrv-mux::write_ivf` / `write_obu_stream` to emit bare AV1 elementary streams. Mastering-tier inputs are rejected at the top of `cmd_mux` before any rav1e initialisation, and the progressive muxer automatically promotes `stco` → `co64` and `mdat` → `largesize` headers when offsets or payloads exceed the 32-bit ISOBMFF limits.

The `still` command reads one selected frame from either delivery-tier `.qdrv32` or mastering-tier `.qdrv64` input. Delivery frames are encoded directly through the single-frame AV1 path; mastering frames first pass through `qdrv-encode::transcode_frame_with_options` so the output AV1 item uses the same delivery-domain PQ representation as the motion mux outputs. `qdrv-mux::write_avif` then writes `ftyp`, `meta`, and `mdat` boxes for a single-image AVIF: major brand `avif`, compatible brands `avif`/`mif1`/`miaf`/`avis`, primary `av01` item, `ispe` dimensions, `pixi` 12-bit RGB component depth, `av1C`, HDR `colr nclx`, and an optional QDRV `mime` metadata item with content type `application/qdrv+json`. The CLI writes through the same `.part.<pid>` atomic-replace pattern used by the other authoring commands.

The `probe-stream` command is the read path: it detects the input format (ISOBMFF container, IVF, or raw OBU stream), recovers the AV1 samples via `qdrv-mux::extract_av1_samples` (a bounds-checked demuxer handling both progressive sample tables and `moof`/`trun` fragments), extracts the embedded metadata OBUs with `qdrv-codec::extract_all_qdrv_metadata`, decodes each with `qdrv-meta::binary::decode_dynamic_binary`, and prints a per-frame summary.

### 2.1 ACES/OpenEXR Export

`qdrv aces-export` is implemented in `qdrv-tool/src/aces_export.rs`. It accepts delivery-tier `.qdrv32` PQ data and mastering-tier `.qdrv64` linear-nits data, maps the input Rec. 2020 RGB into ACES AP0 through `qdrv-core::aces::rec2020_to_aces_ap0`, and writes a numbered OpenEXR sequence through the pure-Rust `exr` crate.

The target set is:

- `aces2065-1`: scene-linear ACES AP0 interchange, no RRT/ODT.
- `rec709-100nit`: `apply_rrt` followed by `apply_odt_rec709_100nit`.
- `rec2020-1000nit`: ACES v1.3 Rec.2020 ST2084 1000 nit published RRT+ODT transform.
- `rec2020-4000nit`: ACES v1.3 Rec.2020 ST2084 4000 nit published RRT+ODT transform.

The ACES code in `qdrv-core::aces` cites the AMPAS ACES Core v1.3 CTL reference paths used for the RRT, Rec.709 ODT, Rec.2020 HDR output transforms, AP0/AP1 matrices, and D60-to-D65 chromatic adaptation. The CLI path writes each EXR through a `.part.<pid>` temporary file, syncs that file handle, and then renames it into place. The ACES2065-1 interchange contract is covered by a regression test that writes OpenEXR, reads it back via `exr`, converts back into delivery PQ, and checks QDRV's PSNR and DeltaE76 metrics.

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
- `spherical_projection` (optional `SphericalProjection`: equirectangular, cubemap, or EAC)

Validation enforces finite values, monotonic structures where required, and range constraints.

### 5.2 Runtime behaviour

`qdrv-decode/src/tone_map.rs` can apply:

- local grid gain/offset sampling
- display adaptation by model class
- ambient lux boost policy
- gaming-profile temporal control and anti-pumping
- deterministic render quantisation when requested

Creator intent lock can suppress non-authorial adaptation.

### 5.3 Object-based and spherical regional tone mapping

`qdrv-meta/src/object_meta.rs` provides per-region tone mapping that overrides
the global per-frame curve inside a region:

- `ObjectRegion` — a flat region located by a normalised `BoundingBox`, with
  optional bounded `RegionMotion`.
- `SphericalRegion` — the angular counterpart for 360°/immersive content,
  located by `centre_azimuth`/`centre_elevation` and `angular_width`/
  `angular_height` (radians), carrying the same `tone_map_curve` and `priority`
  as `ObjectRegion`. Containment is antimeridian-aware in longitude and
  pole-bounded in latitude.
- `SphericalProjection` — `Equirectangular`, `Cubemap`, or
  `EquiAngularCubemap`, carried at stream level by
  `OpenDynamicMetadataV2.spherical_projection`.

Per-frame regions live on `ObjectMeta` (`regions` and `spherical_regions`).
`qdrv-decode/src/object_tone_map.rs` resolves the curve for each pixel: with a
projection in effect it un-projects the pixel's raster coordinate to
`(azimuth, elevation)` — linear for equirectangular, via the canonical 3×2 face
grid for cubemap/EAC — and applies the highest-priority spherical region
containing it; otherwise it uses the flat `BoundingBox` regions. Pixels outside
all regions fall back to the global frame curve.

Flat regions may carry `motion`. The implemented motion variants are:

- `RegionMotion::Static { frame_count }`
- `RegionMotion::Translate { dx_per_frame, dy_per_frame, frame_count }`
- `RegionMotion::PiecewiseLinear { keyframes }`, where each keyframe carries
  `frame_delta`, `dx`, and `dy`

`ObjectMeta::resolve_curve_at_frame` computes a frame delta from
`ObjectMeta.frame_index` and applies the moved box before priority resolution.
`ObjectMeta::validate` rejects zero-frame spans, non-finite translation deltas,
piecewise keyframe lists shorter than two entries or longer than 64 entries,
first keyframe frame_delta must be 0, non-increasing keyframe deltas, and moved boxes that
leave the unit square. The decoder still rejects mismatched object/dynamic frame
indices unless every flat region has an active bounded motion descriptor for the
rendered frame.

`qdrv object-motion` is the authoring helper for this metadata. It reads an
`ObjectMeta` JSON file, selects a rectilinear region by `--region-id`, writes a
`static`, `translate`, or `piecewise-linear` descriptor, validates the complete
document, and emits the result through the same atomic output path as other CLI
sidecar writers.

### 5.4 Multi-frame temporal anti-flicker (sliding-window stabilisation)

To suppress low-frequency luminance pumping (gradual brightness drifts over multiple frames) that can escape single-frame smoothing, QDRV supports a multi-frame integration window.

- **Data Model**: Configured through `TemporalConstraint` via the optional `integration_window_frames` field. If omitted, it defaults to a window of 12 frames.
- **Algorithm**: `TemporalStateManager` maintains a sliding history (`VecDeque<f32>`) of recent input frame luminance values. It computes a running mean and running variance over the window:
  - If the running standard deviation ($\sigma$) is low ($\sigma < \text{STABLE\_LUMA\_EPSILON}$ where $\text{STABLE\_LUMA\_EPSILON} = 0.015$), the scene is determined to be temporally stable.
  - An additional proportional dampening factor is applied:
    $$D = 1.0 - (1.0 - \text{STABLE\_LUMA\_DAMP}) \times \left(1.0 - \frac{\sigma}{\text{STABLE\_LUMA\_EPSILON}}\right)$$
    where $\text{STABLE\_LUMA\_DAMP} = 0.5$. The gain adjustment delta is scaled by $D$.
  - When the scene is perfectly stable ($\sigma = 0$), $D$ is $0.5$ (full extra damping). If a scene transition or significant motion occurs, the variance increases beyond the stable threshold, and the stabiliser returns to standard tracking speed ($D = 1.0$) to avoid lagging behind scene transitions.
- **Interaction and Precedence**:
  - High-frequency flicker (manifesting over 2-3 frames) is handled by the per-frame damping parameters (`anti_pumping_strength` and `max_global_gain_delta_per_frame`).
  - Low-frequency pumping is managed by the sliding window. The window dampening operates after the per-frame maximum delta clamping but blends the final step toward the previous frame's gain. This ensures that the two controls compose harmoniously: high-frequency changes are capped per-frame, while slow, systematic drift is actively damped when the overall scene content is static.

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

### 7.4 ISOBMFF mux robustness

`qdrv-mux/src/lib.rs` includes:

- safe size arithmetic for box construction
- automatic `stco` vs `co64` chunk-offset selection
- `mdat` `largesize` handling for payloads exceeding 32-bit box size limits
- HDR colour signalling via a `colr` `nclx` box (BT.2020 primaries,
  SMPTE ST 2084 transfer, BT.2020 NCL matrix, full pixel range) so MP4-
  level players that read HDR characteristics from the container — rather
  than parsing the AV1 OBU — see the correct interpretation

`qdrv-mux/src/avif.rs` adds the AVIF still-image branch. It writes a root
`meta` box with `pitm`, `iloc`, `iinf`, `iref`, and `iprp`, associates the
primary `av01` item with `ispe`, `pixi`, `av1C`, and `colr` properties, and
stores QDRV metadata as a linked `application/qdrv+json` MIME item.

### 7.5 External-tool interoperability

The mux outputs are standard AV1-in-ISOBMFF / CMAF and AV1 elementary
streams, so they interoperate with the established AV1 toolchain rather than
requiring QDRV-specific readers. Observed acceptance against generated
output:

- `ffprobe` / `ffmpeg` identify and decode the `mp4`, `fmp4`, `cmaf`, `ivf`,
  and `obu` outputs as AV1 (Professional profile, 12-bit 4:4:4, BT.2020
  primaries, SMPTE ST 2084 transfer, BT.2020 NCL matrix).
- `dav1d` decodes the IVF and raw-OBU elementary streams.
- GPAC `MP4Box` parses the progressive MP4, fragmented MP4, and CMAF outputs
  and reports the AV1 track (`av01.2.04M.00`; the CMAF output carries the
  `cmfc` brand).
- GPAC `MP4Box` parses AVIF still output, reports the primary `av01` image item
  and the `application/qdrv+json` metadata item, and can dump both items.
- `dav1d` decodes the AV1 image item dumped from AVIF still output.
- Shaka Packager repackages the progressive MP4 into MPEG-DASH (`.mpd`) and
  HLS (`.m3u8`) without re-encoding, preserving the in-bitstream ITU-T T.35
  metadata OBUs.

These are manual interoperability observations against generated output, not
part of the gated workspace test suite.

### 7.6 In-browser decode build (`qdrv-decode-wasm`)

`qdrv-decode-wasm` compiles the codec-free parts of the pipeline to
`wasm32-unknown-unknown` for browser playback. It deliberately excludes the
native codecs (`rav1e`, `dav1d`, `fpzip`, `zfp`): the browser supplies the AV1
decode, and the crate links `qdrv-codec` with `--no-default-features` so only
the pure-Rust metadata-OBU parser is included. The target-independent core
functions are unit-tested under the normal workspace gates; the thin
`wasm-bindgen` exports are compiled only for `wasm32`.

Exported surface:

- `parse_qdrv32_container` — bounds-checked delivery-container parsing that
  returns validated static/dynamic metadata plus per-frame AV1 payload ranges.
- `extract_stream_metadata` — in-bitstream ITU-T T.35 metadata recovery for
  IVF/OBU streams.
- `wrap_av1_still_as_avif` — wraps one `.qdrv32` AV1 still-picture payload as
  a single-image AVIF (through `qdrv-mux::write_avif`) so browser still-image
  decoders can play payloads that `VideoDecoder` rejects at its keyframe gate.
- `yuv_ncl_to_pq_rgb` — the Rec. 2100 non-constant-luminance bridge from a
  decoded 4:4:4 Y'CbCr frame to the PQ RGB layout the tone mapper expects.
- `build_tone_curve_lut` — CPU-evaluated tone-curve lookup table so the WebGPU
  compute-shader path matches the native curve evaluation exactly.
- `tone_map_frame` — the full per-frame tone map, including the flat and
  360°/immersive per-region paths.

The runnable demonstration page in `examples/web/` prefers the WebGPU compute
shader (`tone-map.wgsl`) and falls back to the wasm CPU path. Browser playback
is experimental and browser-dependent (AV1 Professional profile, 12-bit
4:4:4); it is validated in a browser rather than by the gated test suite.

## 8. Limitations and External Dependencies

- Open HDR10+ profile exports are compatibility-oriented and not certified
- Certified Dolby Vision bitstream packaging is external and proprietary
- `zfp` mastering compression is optional and feature-gated
- VMAF-HDR high-fidelity scoring depends on external tooling; deterministic fallback remains available
- Raw codec mode is test-focused
- `qdrv convert` currently requires mastering-tier input
- In-browser playback (`qdrv-decode-wasm` plus `examples/web/`) is an experimental, browser-dependent demonstration and is not part of the gated verification suite

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
