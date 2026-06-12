# QDRV Roadmap

Author: Michael Lauzon <qdrv2026@gmail.com>

This document tracks proposed future work for QDRV. It is implementation-first: each item describes what the feature is, why it earns space on the roadmap, the technical surface it would touch in the current workspace, and the dependencies or blockers that need resolution before substantive work begins. Items are ordered by expected operator impact rather than by implementation difficulty.

The roadmap is consultative, not contractual. Priorities will shift as operator feedback arrives, as external dependencies stabilise, and as time and resources allow. The verification gates documented elsewhere in the project — `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the full `cargo test --workspace` suite under both default and `qdrv-codec/zfp` feature configurations — remain non-negotiable for any feature listed here. Nothing ships until those gates pass cleanly and any new behaviour carries regression coverage in line with the patterns already established in the workspace.

## 1. Spatial metadata for 360° and immersive video — shipped

Implemented and verified. `qdrv-meta` provides the `SphericalProjection` enum, the `SphericalRegion` type, the stream-level `OpenDynamicMetadataV2.spherical_projection` field, and the per-frame `ObjectMeta.spherical_regions` field; `qdrv-decode::object_tone_map` un-projects each pixel and applies the highest-priority spherical region, with equirectangular, cubemap, and equi-angular cubemap layouts all supported (longitude wraps across the antimeridian; latitude is pole-bounded). The projection enums, angular ranges, and pole/seam rules are documented in `docs/QDRV_SPEC.md` §5.4 and `docs/QDRV_TECHNICAL_REFERENCE.md` §5.3, and a checked-in fixture (`test-vectors/spherical-region.objectmeta.json`) exercises the path end-to-end. The planning-stage decision on signalling was settled in favour of the in-stream metadata field; container-level `sv3d`/`proj` delegation remains an optional future addition. This entry is retained as a record of completed work.

## 2. WebAssembly build of `qdrv-decode` for in-browser playback — shipped

Implemented and verified on the Rust side; the browser-runtime layer is validated in a browser rather than by the native gates. The `qdrv-decode-wasm` companion crate is codec-free by design (the browser's WebCodecs `VideoDecoder` decodes the AV1 payload) and exposes a small `wasm-bindgen` surface: `tone_map_frame` (the full per-frame tone map, including the flat and 360°/immersive per-region paths from item 1), `yuv_ncl_to_pq_rgb` (the BT.2020 non-constant-luminance bridge from a decoded 4:4:4 Y'CbCr frame to PQ RGB), `build_tone_curve_lut` (a CPU-evaluated lookup table so the GPU path matches the native curve evaluation exactly), `extract_stream_metadata` (in-bitstream ITU-T T.35 recovery through the pure-Rust OBU parser, which `qdrv-codec` now exposes under `--no-default-features`), `parse_qdrv32_container` (a fully bounds-checked delivery-container parser for direct `.qdrv32` playback), and `wrap_av1_still_as_avif` (which wraps a `.qdrv32` AV1 still-picture payload as a single-image AVIF so browser still-image decoders can play it where `VideoDecoder` rejects still-picture temporal units). The per-pixel tone map also runs as a WebGPU compute shader (`examples/web/tone-map.wgsl`) with the wasm path as its fallback, and `examples/web/` carries both the synthetic demo page and the experimental WebCodecs player for IVF and `.qdrv32` inputs.

The remaining caveat is environmental, not architectural: playback of real streams depends on the browser decoding AV1 Professional profile (12-bit 4:4:4); browsers limited to 8-bit 4:2:0 AV1 reject the stream at `VideoDecoder.configure()`. The target-independent core is unit-tested under the normal workspace gates, and the wasm32 build compiles warning-free. This entry is retained as a record of completed work.

## 3. Per-object motion tracking in dynamic metadata — shipped

**Goal.** Extend `ObjectMeta` so each region can carry a motion vector or trajectory descriptor across frames, allowing the per-region tone-mapping curve to follow a moving object automatically rather than requiring per-frame reauthoring.

**Why this earns this slot.** The current `ObjectMeta` model is per-frame and stateless: a region painted on frame 100 has no relationship to its visual counterpart on frame 101 unless the operator copies it forward by hand. For long-form content with consistent subjects — a presenter, a vehicle, a brand mark, a recurring product placement — this is real labour. Lightweight motion metadata reduces the per-frame authoring burden materially without complicating the rendering side, and it composes cleanly with the existing `priority` and `tone_map_curve` fields.

**Implemented surface.**

- `motion: Option<RegionMotion>` is implemented on `ObjectRegion`, with `Static { frame_count }`, `Translate { dx_per_frame, dy_per_frame, frame_count }`, and `PiecewiseLinear { keyframes }` variants. The bounded keyframe shape covers non-linear paths through explicit segmented interpolation rather than an unbounded polynomial.
- Decoder integration uses `qdrv-meta::object_meta::ObjectMeta::resolve_curve_at_frame` from `qdrv-decode::object_tone_map` so flat regions advance their bounding box by the motion descriptor when a later frame within the authored span is rendered.
- Validation in `qdrv-meta::object_meta::ObjectMeta::validate` rejects degenerate motion fields: zero-frame spans, non-finite deltas, malformed piecewise keyframes, and moved bounding boxes that leave the unit square.
- `qdrv-tool` provides `qdrv object-motion`, which ingests an `ObjectMeta` JSON document, selects a rectilinear region by ID, writes a static/translated/piecewise-linear motion descriptor, validates the result, and writes the updated JSON through the existing atomic sidecar path.

**Verification.**

- `qdrv-meta` tests cover JSON round-trip, translated lookup, piecewise-linear interpolation, and validation failures for off-canvas, non-finite, zero-frame, and malformed-keyframe motion.
- `qdrv-decode` tests cover applying active motion from an authored keyframe and rejecting expired motion metadata.
- `qdrv-tool` tests cover `object-motion` CLI parsing and output generation for translated and piecewise-linear descriptors.

**Complexity outcome.** Moderate. The metadata extension and decoder integration stayed small; the main design choice was settling on bounded segmented interpolation so validation remains finite and deterministic.

## 4. ACES RRT and ODT output transforms — shipped

Implemented and verified. `qdrv-core::aces` now exposes `apply_rrt`, `apply_odt_rec709_100nit`, `apply_odt_rec2020_1000nit`, and `apply_odt_rec2020_4000nit`; the implementation ports the AMPAS ACES Core v1.3 CTL RRT/ODT paths and cites the CTL source files beside the constants and transforms. `qdrv-tool` provides `qdrv aces-export`, which exports delivery-tier `.qdrv32` or mastering-tier `.qdrv64` frames as a numbered OpenEXR sequence with targets `aces2065-1`, `rec709-100nit`, `rec2020-1000nit`, and `rec2020-4000nit`.

The OpenEXR path uses the pure-Rust `exr` crate with default features disabled, writes through a `.part.<pid>` temporary file, syncs the file handle, and atomically renames into the requested output filename. The ACES2065-1 interchange path has a regression test that writes OpenEXR, reads it back through the same pure-Rust EXR stack, converts back to delivery PQ, and checks QDRV's PSNR and DeltaE76 fidelity metrics. Display-rendered Rec.709/Rec.2020 ODT targets are documented as output transforms rather than reversible interchange.

**Goal.** Complete the ACES integration in `qdrv-core::aces` by adding the standard ACES Reference Rendering Transform (RRT) and the canonical Output Display Transforms (ODTs), so a QDRV delivery file can produce ACES-compliant output for ingestion into ACES-aware downstream pipelines without an external conversion step.

**Why this earns this slot.** `qdrv-core::aces` already implements the ACES AP0 (ACES2065-1) and AP1 (ACEScg) *input* transforms — the conversions *into* QDRV's working space. The output side is missing, which means ACES-based mastering houses can author into QDRV but cannot cleanly export back to ACES for archival or for handoff to downstream colour-grading pipelines. Closing the loop turns QDRV into a credible end-to-end alternative to existing ACES workflows, rather than an inbound-only converter.

**Technical surface.**

- Functions in `qdrv-core::aces` for the RRT (`apply_rrt`) and the canonical ODTs, including `apply_odt_rec709_100nit`, `apply_odt_rec2020_1000nit`, and `apply_odt_rec2020_4000nit`.
- A CLI subcommand `qdrv aces-export` in `qdrv-tool` that wraps the export pipeline (`.qdrv32`/`.qdrv64` to ACES OpenEXR sequence, RRT plus the operator's chosen ODT when applicable) and writes the output through the same `.part.<pid>` atomic-replace pattern used by other QDRV writers.
- Fidelity coverage for the QDRV to ACES2065-1 OpenEXR to QDRV round trip so ACES interchange preserves the documented PSNR and DeltaE76 targets.

**Dependencies and blockers.**

- The ODT matrix coefficients come from the official ACES specification (SMPTE ST 2065-1 and the AMPAS ACES Core v1.3 CTL reference implementation) rather than being re-derived from first principles. The documentation pattern established in `qdrv-core::aces` now includes citations for the RRT, ODTs, AP0/AP1 matrices, and chromatic-adaptation constants.
- OpenEXR write support is provided by the pure-Rust `exr` crate. No C dependency and no in-repository EXR byte writer are used for this feature.

**Complexity outcome.** Moderate. The RRT/ODT port is numerically dense, but the existing AP0/AP1 matrix module gave the right home for it; the OpenEXR integration is handled through a maintained pure-Rust crate rather than bespoke file-format code.

## 5. AVIF still-image profile — shipped

**Status.** Implemented through `qdrv still` and `qdrv-mux::write_avif`.

**Goal.** Define a single-frame AVIF output profile that produces an AV1-encoded HDR still image suitable for photographers and colourists working on floating-point HDR stills rather than motion content.

**Why this earns this slot.** AVIF has rapidly become the practical floating-point-adjacent still-image format for HDR photography, but operators authoring in floating-point pipelines still lose precision at the encode step because most AVIF tooling assumes integer-domain mastering input. QDRV is already an AV1 mux, and the mastering-tier Float64 path already preserves precision through to the codec boundary. A still-image profile lets photographers produce AVIF assets that retain the mastering-tier precision QDRV is built around, without leaving the workspace's verification regime.

**Delivered technical surface.**

- `qdrv-mux::write_avif` emits a single-image ISOBMFF/AVIF file with `avif` as the major brand and `avif`, `mif1`, `miaf`, and `avis` as compatible brands, while preserving HDR `colr nclx` signalling.
- `qdrv still` accepts delivery-tier `.qdrv32` and mastering-tier `.qdrv64` inputs, selects a frame with `--frame-index`, writes through the `.part.<pid>` atomic-replace path, and exposes `--quantizer`, `--speed`, and `--deterministic`.
- Tests cover the AVIF writer structure plus command-level delivery and mastering exports. External validation uses GPAC MP4Box to parse and dump AVIF items and dav1d to decode the extracted AV1 image item.

**Metadata and verification notes.**

- QDRV metadata is stored as an `application/qdrv+json` MIME metadata item linked to the primary AV1 item from the AVIF `meta` box. The JSON preserves source container version, source tier, selected frame index, static metadata, and dynamic metadata.
- Mastering inputs are transcoded through the existing mastering-to-delivery path before AV1 still-picture encoding, so the AVIF item uses the same delivery-domain PQ representation as the motion mux outputs.

## 6. Multi-frame integration buffer for anti-flicker — shipped

Implemented and verified. `TemporalStateManager` in `qdrv-decode::tone_map` now keeps a sliding ring buffer of recent frame luminance (a `VecDeque<f32>` history) together with running mean and variance aggregates, and consults them alongside the existing single-frame IIR state. When the running standard deviation over the window shows a stable scene, the controller blends the per-frame gain delta back toward the previous gain, suppressing the low-frequency "inhale-and-exhale" luminance drift that escapes single-frame smoothing; an unstable window leaves the existing high-frequency anti-pumping behaviour unchanged.

The window is configured through the new optional `TemporalConstraint.integration_window_frames` field (`Option<u8>`), with a documented default of 12 frames when the field is absent; validation rejects an explicit zero. The interaction with `anti_pumping_strength` and the damping formula are documented in `docs/QDRV_TECHNICAL_REFERENCE.md` §5.4, and regression tests in `qdrv-decode::tone_map` compare windowed against non-windowed gain trajectories on drifting content. This entry is retained as a record of completed work.

## 7. AI-assisted dynamic-metadata authoring

**Goal.** Add an optional machine-learning path that analyses a QDRV frame range and proposes dynamic metadata — `OpenDynamicMetadataV2` tone curves and `ObjectMeta` regions — so operators can start from a generated draft instead of authoring every scene and every region by hand. The inference would live in a feature-gated `qdrv-ai` sidecar crate that never becomes a core dependency and never touches the deterministic conversion or render paths, with all model output treated as an advisory suggestion the operator reviews and edits rather than a silent transform.

**Why this earns this slot.** The dynamic-metadata model already exists — `OpenDynamicMetadataV2`, the tone-curve structures, and `ObjectMeta` are all implemented — but today every curve and every region is authored manually, which is the single most labour-intensive part of using QDRV well. A model that drafts that metadata is the one "add AI" idea that fits the format's actual architecture. Unlike AV2 (see item #8), the pure-Rust infrastructure for AI inference already exists through `tract` — the pure-Rust ONNX engine from Sonos — keeping the clean-build, offline-reproducible, and near-zero-`unsafe` properties the workspace depends on. The primary blocker is obtaining a model trained for QDRV's HDR pixel domain rather than building the integration layer itself.

**Technical surface.**

- A new feature-gated `qdrv-ai` companion crate that performs inference through `tract` — the pure-Rust ONNX engine from Sonos — rather than the `ort` ONNX Runtime wrapper. `tract` keeps the clean-build, offline-reproducible, and near-zero-`unsafe` properties the workspace depends on; `ort` links the native C++ ONNX Runtime and its FFI surface, which would expand the audit scope, break offline and reproducible builds, and end the workspace's current two-`unsafe`-block guarantee. The pure-Rust path is the design constraint here, not an implementation detail.
- A `qdrv-tool` subcommand, tentatively `qdrv suggest-metadata`, that runs the model over a `.qdrv32` or `.qdrv64` frame range and writes an `OpenDynamicMetadataV2` or `ObjectMeta` JSON document through the same `.part.<pid>` atomic-replace pattern every other QDRV writer uses. The emitted document is explicitly marked as machine-suggested and remains subject to the existing creator-intent-lock semantics, so generated metadata can never silently override authored intent.
- A pixel-domain contract: any model must operate on QDRV's actual data — 12-bit or floating-point HDR in PQ or linear light — not the 8-bit SDR range that off-the-shelf vision models assume. Inputs normalised to `[0.0, 1.0]` over an SDR distribution do not transfer to HDR content and would produce unusable suggestions.

**Dependencies and blockers.**

- The binding blocker is the model itself. No pre-trained network exists for the real task — reading HDR floating-point frames and proposing tone curves and regions — and SDR-domain models do not transfer. This requires either training a model on HDR-domain data or sourcing one. However, unlike AV2, the pure-Rust inference engine (`tract`) already exists and is GPL-compatible, so the integration work can proceed once a suitable model is obtained.
- Determinism. Inference is not bit-reproducible across hardware, thread counts, or runtime versions, so the feature must stay outside `--deterministic`, outside the fidelity-contract paths, and outside the default build. Its output is a draft for review, never a step in a reproducible transcode.
- Model weights are large binary artefacts that do not belong in the repository or in the deterministic verification gates. The sidecar's tests must exercise the integration with tiny fixture models or by mocking the inference boundary, so the workspace gates stay fast and reproducible whether or not a real model is present.

**Estimated complexity.** Moderate once a suitable model exists. The in-workspace integration mirrors patterns the codebase already uses: a feature-gated crate, a single `tract` inference call, and a tool subcommand emitting reviewable JSON. Unlike AV2, the Rust infrastructure layer is available today; the remaining work is obtaining and validating a model trained for QDRV's HDR pixel domain.

## 8. AV2 delivery-tier codec support

**Goal.** Add AV2 — the Alliance for Open Media's successor to AV1 — as an alternative delivery-tier codec alongside the current AV1 path, so a `.qdrv32` delivery stream can carry an AV2 bitstream once a production-grade Rust AV2 encoder exists, taking advantage of AV2's improved compression efficiency at equivalent perceptual quality.

**Why this earns this slot.** AV2 is positioned to deliver materially better compression than AV1 at the same visual quality, which maps directly onto QDRV's delivery-tier mandate: smaller files for the same 12-bit 4:4:4 HDR content. However, it sits at the bottom of the list because the Rust encoder ecosystem does not yet exist in a form QDRV can use. The decode ecosystem has moved first: rav2d, a BSD-2-Clause Rust AV2 decoder, is GPL-compatible and could eventually slot in beside the existing dav1d path. But rav1e is AV1-only, and there is no production-grade Rust AV2 encoder. Until that gap closes, QDRV could in principle read AV2 yet not produce it, which inverts the format's purpose. Unlike AI (see item #7), where the pure-Rust infrastructure exists today through `tract`, AV2 has no viable pure-Rust encode path. This is a watch-and-track item: progress is gated on an external dependency maturing, not on QDRV design effort.

**Technical surface.**

- A way for the delivery-tier container to signal an AV2 payload distinctly from AV1 — the header already carries a codec byte — so readers reject a codec they cannot decode rather than misparsing it as AV1.
- A `qdrv-codec` encode path mirroring the current `av1` and `temporal` modules once a Rust AV2 encoder is available, holding the same 12-bit 4:4:4, Rec. 2020 primaries, SMPTE ST 2084 transfer contract the AV1 path already enforces.
- A decode path wrapping rav2d behind the same buffer-reuse and error-handling shape as the existing dav1d integration in `qdrv-codec::av1`.
- An assessment of whether the ITU-T T.35 metadata-OBU carriage in `qdrv-codec::metadata_obu` transfers to AV2 unchanged. AV2 inherits AV1's OBU framing, so the carriage is expected to port closely, but the OBU header layout, metadata type, and trailing-marker assumptions must be re-verified against the AV2 bitstream specification rather than assumed.

**Dependencies and blockers.**

- The binding blocker is a production-quality Rust AV2 encoder. rav1e does not encode AV2, and the reference AVM encoder is a C/C++ research codebase rather than a library QDRV would link against under its current pure-Rust-where-possible dependency policy. No substantive encode work begins until a viable Rust encoder exists and clears the same dependency scrutiny already applied to `fpzip-rs` and `zfp-sys-cc`.
- rav2d is early-stage; its decode conformance and API stability need evaluation before it is wired into the verification gates. A decoder that cannot yet round-trip the full feature set QDRV emits is not a dependency QDRV can rely on.
- AV2 support is strictly additive: it must not disturb the AV1 delivery path, which remains the default. The container's codec signalling and reader validation are the mechanism that keeps the two codecs from being confused.

**Estimated complexity.** Currently unscoped, because it is gated on external tooling that does not yet exist. Once a Rust AV2 encoder exists, the in-workspace integration mirrors the established AV1 path and is moderate; the real cost sits entirely outside QDRV, in the maturity of the Rust AV2 encode and decode ecosystem. Until that matures, this item stays in watch-and-track status rather than active development.

## Contribution scope

Roadmap items are open to external contribution provided the proposal goes through a short design discussion before substantive code is written. The pattern that has worked elsewhere in this workspace is a short design note (one to three paragraphs) covering the metadata shape, the validation rules, and the test fixtures, posted as a draft pull request or as a discussion item, before any implementation lands.

The verification gates are not negotiable: every roadmap item must finish with `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` (both default and `qdrv-codec/zfp` feature configurations), and the full `cargo test --workspace` suite passing cleanly, with regression coverage for any new behaviour written in the same style as the existing tests in the affected crate.
