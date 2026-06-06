# QDRV Roadmap

Author: Michael Lauzon <qdrv2026@gmail.com>

This document tracks proposed future work for QDRV. It is implementation-first: each item describes what the feature is, why it earns space on the roadmap, the technical surface it would touch in the current workspace, and the dependencies or blockers that need resolution before substantive work begins. Items are ordered by expected operator impact rather than by implementation difficulty.

The roadmap is consultative, not contractual. Priorities will shift as operator feedback arrives, as external dependencies stabilise, and as time and resources allow. The verification gates documented elsewhere in the project — `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the full `cargo test --workspace` suite under both default and `qdrv-codec/zfp` feature configurations — remain non-negotiable for any feature listed here. Nothing ships until those gates pass cleanly and any new behaviour carries regression coverage in line with the patterns already established in the workspace.

## 1. Spatial metadata for 360° and immersive video

**Goal.** Extend `OpenDynamicMetadataV2` so the existing per-region tone-mapping infrastructure (`LocalToneMapGrid`, `ObjectMeta`, and the v2 scene constraints) maps coherently onto spherical and equirectangular projections used by 360° and immersive video pipelines.

**Why this earns the top slot.** 360° and head-mounted-display content has measurable adoption among mastering and post-production teams who currently fall back to per-frame manual grading because their tone-mapping tools assume rectilinear projection. QDRV's per-region creative-intent model translates naturally to spherical regions once the projection geometry is communicated explicitly. This is a substantive content-category expansion that the integer HDR ecosystem cannot match cleanly: HDR10 and HDR10+ have no spatial vocabulary at all, and Dolby Vision's spatial metadata is proprietary and gated behind certification.

**Technical surface.**

- A new optional field on `OpenDynamicMetadataV2`, tentatively `spherical_projection: Option<SphericalProjection>`, where the enum covers at least equirectangular (ERP), cubemap, and equi-angular cubemap (EAC) layouts.
- A new `SphericalRegion` shape in `qdrv-meta::object_meta` describing centre azimuth and elevation, angular width, and angular height, paired with the same `priority` and `tone_map_curve` fields the existing `ObjectRegion` already carries.
- Decoder support in `qdrv-decode::object_tone_map` so the per-pixel lookup understands spherical coordinates rather than flat raster coordinates in `[0.0, 1.0]²`.
- Spec additions under `docs/QDRV_SPEC.md` documenting the projection enums, angular coordinate ranges, and the singularity behaviour at the poles.

**Dependencies and blockers.**

- A small reference corpus of spherical fixtures, beyond the current `16 × 4` ramp test vectors, so the codec round-trip tests can validate the new path end-to-end without relying on operator-supplied content.
- A decision on whether QDRV signals its own projection metadata or delegates to the MP4 `sv3d` and `proj` boxes for container-level interoperation. Delegation is preferable for muxer simplicity; an in-stream signalling field would let `.qdrv32` files declare projection without a container wrapper. The two paths are not mutually exclusive but the default behaviour should be settled before authoring begins.

**Estimated complexity.** Moderate. The metadata-schema changes are straightforward additive extensions and follow the existing v2 pattern. The harder work is in `qdrv-decode`, where the sampling kernel must account for spherical distortion at the poles without introducing bias near the equator.

## 2. WebAssembly build of `qdrv-decode` for in-browser playback

**Goal.** Ship a WebAssembly build of `qdrv-decode` and a thin JavaScript wrapper so a QDRV delivery-tier `.qdrv32` file can be played directly in a browser via the WebCodecs API and WebGPU compute shaders, without requiring a server-side conversion step or a desktop installation.

**Why this earns the second slot.** The browser is the most consequential video-distribution surface for open formats. A QDRV delivery file that plays in Chrome, Firefox, or Safari with no plug-in installation removes the single largest adoption barrier the format currently has: operators today need a desktop tool to view their own output, which is a real friction for sharing samples, demonstrating workflows, or embedding QDRV in web-based review pipelines. WebCodecs already exposes AV1 decoding in the major browsers; the missing piece is the QDRV metadata layer and the per-pixel tone-mapping kernel.

**Technical surface.**

- A `qdrv-decode-wasm` companion crate (or a `wasm32-unknown-unknown` cargo target on the existing crate) that exposes a small `wasm-bindgen` surface for parsing QDRV containers and producing tone-mapped frames.
- Replacement of the `rav1e`/`dav1d` AV1 paths with the browser-supplied `VideoDecoder` (WebCodecs) interface, since the native codecs do not build for `wasm32` and would dominate the artefact size even if they did.
- A WebGPU compute-shader equivalent of `qdrv-decode::tone_map::tone_map_frame` so the per-pixel tone-mapping work runs on the GPU rather than across the JavaScript-to-WebAssembly call boundary.
- An example page under a new `examples/web/` directory demonstrating playback of one of the checked-in test vectors, with a CPU fallback path for browsers that do not yet expose WebGPU.

**Dependencies and blockers.**

- The size of the `.wasm` artefact materially affects adoption. Aggressive `wasm-opt` passes, dead-code elimination, and a conservative `panic = "abort"` profile should keep the published artefact under approximately one megabyte compressed, which is the practical ceiling for first-paint scenarios.
- WebGPU support is uneven across browsers: Safari shipped support recently and Firefox is still rolling out. The fallback path should use plain WebGL 2 or a CPU implementation so unsupported browsers degrade rather than fail outright.
- The fidelity contract surface does not transfer to the browser unchanged. PSNR and SSIM can be computed in shader code; VMAF-HDR cannot. The browser build should expose only the metrics that work without external tooling.

**Estimated complexity.** Moderate. Most of the work is integration with browser APIs rather than algorithmic. The compute-shader port of the tone mapper is the single largest chunk; the rest is bindings and packaging.

## 3. Per-object motion tracking in dynamic metadata

**Goal.** Extend `ObjectMeta` so each region can carry a motion vector or trajectory descriptor across frames, allowing the per-region tone-mapping curve to follow a moving object automatically rather than requiring per-frame reauthoring.

**Why this earns this slot.** The current `ObjectMeta` model is per-frame and stateless: a region painted on frame 100 has no relationship to its visual counterpart on frame 101 unless the operator copies it forward by hand. For long-form content with consistent subjects — a presenter, a vehicle, a brand mark, a recurring product placement — this is real labour. Lightweight motion metadata reduces the per-frame authoring burden materially without complicating the rendering side, and it composes cleanly with the existing `priority` and `tone_map_curve` fields.

**Technical surface.**

- A new `motion: Option<RegionMotion>` field on `ObjectRegion`, where `RegionMotion` is an enum covering at least `Static`, `Translate { dx_per_frame: f32, dy_per_frame: f32 }`, and a short piecewise-linear or polynomial spline for non-linear paths.
- Decoder integration so `qdrv-decode::object_tone_map::ObjectMeta::resolve_curve_at` advances the region's bounding box by the motion descriptor when a frame index between authored keyframes is rendered.
- Validation in `qdrv-meta::object_meta::ObjectMeta::validate` so degenerate motion fields — non-finite deltas, off-canvas trajectories, or splines that violate the unit-square containment rule mid-segment — are rejected at the schema boundary rather than at render time.

**Dependencies and blockers.**

- A reference encoder workflow needs to exist before this feature is useful end-to-end. QDRV does not currently author object regions automatically; an upstream tool would need to produce them. A small CLI helper in `qdrv-tool` that ingests an `ObjectMeta` JSON document and interpolates motion across a frame range would be a sensible companion deliverable.
- The schema for `RegionMotion` is the substantive design call. Polynomial splines give expressive flexibility but cost more to validate and document; pure piecewise-linear translations are simple but limit the artistic vocabulary. A two-stage approach — ship `Translate` first, add `Spline` later behind an explicit schema-version bump — keeps the surface area manageable.

**Estimated complexity.** Moderate. The metadata extension is small. The decoder integration is mechanically straightforward. The time sink is the design call on the motion-vector schema and the corresponding validation rules.

## 4. ACES RRT and ODT output transforms

**Goal.** Complete the ACES integration in `qdrv-core::aces` by adding the standard ACES Reference Rendering Transform (RRT) and the canonical Output Display Transforms (ODTs), so a QDRV delivery file can produce ACES-compliant output for ingestion into ACES-aware downstream pipelines without an external conversion step.

**Why this earns this slot.** `qdrv-core::aces` already implements the ACES AP0 (ACES2065-1) and AP1 (ACEScg) *input* transforms — the conversions *into* QDRV's working space. The output side is missing, which means ACES-based mastering houses can author into QDRV but cannot cleanly export back to ACES for archival or for handoff to downstream colour-grading pipelines. Closing the loop turns QDRV into a credible end-to-end alternative to existing ACES workflows, rather than an inbound-only converter.

**Technical surface.**

- New functions in `qdrv-core::aces` for the RRT (`apply_rrt`) and the canonical ODTs, including at minimum `apply_odt_rec709_100nit`, `apply_odt_rec2020_1000nit`, and `apply_odt_rec2020_4000nit`.
- A new CLI subcommand `qdrv aces-export` in `qdrv-tool` that wraps the export pipeline (`.qdrv32` → ACES OpenEXR sequence, RRT plus the operator's chosen ODT) and writes the output through the same `.part.<pid>` atomic-replace pattern used by every other QDRV writer.
- Fidelity contracts for the QDRV → ACES → QDRV round trip so a transcode through ACES interchange preserves the documented PSNR and ΔE76 targets.

**Dependencies and blockers.**

- The ODT matrix coefficients should come from the official ACES specification (SMPTE ST 2065-1 and the ACES CTL reference implementation) rather than being re-derived from first principles. The documentation pattern established in `qdrv-core::aces` — a literal citation of the source for each matrix, with a colocated round-trip test — should be extended to every new ODT added.
- OpenEXR write support is not currently in the workspace. The export path will need either a new dependency on an EXR-writing crate or a minimal in-repo writer for the typical scanline-RGBA case. The dependency review should follow the same scrutiny applied to `fpzip-rs` and `zfp-sys-cc`: pure-Rust where possible; C dependencies tolerated only when there is no practical alternative.

**Estimated complexity.** Moderate. The RRT and ODT mathematics are well-specified and the existing AP0/AP1 implementation gives a template. The time sink is the OpenEXR integration if no acceptable Rust EXR crate is available.

## 5. AVIF still-image profile

**Goal.** Define a single-frame AVIF output profile (either a new `qdrv still` subcommand or a flag on the existing `qdrv mux` command) that produces an AV1-encoded HDR still image suitable for photographers and colourists working on floating-point HDR stills rather than motion content.

**Why this earns this slot.** AVIF has rapidly become the practical floating-point-adjacent still-image format for HDR photography, but operators authoring in floating-point pipelines still lose precision at the encode step because most AVIF tooling assumes integer-domain mastering input. QDRV is already an AV1 mux, and the mastering-tier Float64 path already preserves precision through to the codec boundary. A still-image profile lets photographers produce AVIF assets that retain the mastering-tier precision QDRV is built around, without leaving the workspace's verification regime.

**Technical surface.**

- A new container variant (or a `qdrv mux --still` flag) that emits a single-frame ISOBMFF with the `mif1`, `avif`, and `avis` brands instead of QDRV's existing brand, while preserving the HDR `colr nclx` signalling and the metadata payload through the AVIF metadata-item boxes.
- An accompanying CLI subcommand `qdrv still` that wraps the single-frame encode and writes a conformant `.avif` file through the same atomic-replace pattern used elsewhere in the workspace.
- Round-trip tests against an external AVIF parser, since the workspace does not currently consume AVIF and an in-repo parser would inflate the verification surface unnecessarily.

**Dependencies and blockers.**

- The AVIF brand and box requirements are documented in MIAF and the AVIF specification; the muxer changes should be a small additive set rather than a parallel codepath. The existing `stco`/`co64` and `mdat` `largesize` logic remains correct without modification.
- The metadata story for AVIF stills is the substantive design question. AVIF supports limited metadata through the `iref` and `iprp` boxes; QDRV's `DynamicMeta` and `OpenDynamicMetadataV2` are richer than AVIF accommodates natively. A decision is needed on whether to embed a JSON sidecar inside an `Exif` or `mime` item, or to drop fields that AVIF cannot represent and document the loss in the corresponding loss report.

**Estimated complexity.** Small to moderate. The codec and most of the muxer infrastructure already exist. The work concentrates on the brand-and-box differences, the metadata-embedding decisions, and the round-trip validation harness.

## 6. Multi-frame integration buffer for anti-flicker

**Goal.** Augment the current single-frame temporal anti-pumping controller in `qdrv-decode::tone_map::TemporalStateManager` with a small ring-buffer-based integration window, so low-frequency luminance pumping that escapes single-frame smoothing is also suppressed.

**Why this earns this slot.** The current `TemporalStateManager` is a one-frame IIR with a damping factor. The parameters documented in the workspace (`anti_pumping_strength`, `max_global_gain_delta_per_frame`) capture sensible defaults for high-frequency flicker — the kind that manifests across two or three frames and is visible as flicker proper. They do not catch low-frequency pumping: the gradual brightness drift that occurs over roughly half a second and is visible on long takes as a slow inhale-and-exhale of overall image luminance. A short ring buffer over recent frame luminance, with a proper sliding-window aggregate, would suppress that band without requiring operators to retune the existing per-frame parameters.

**Technical surface.**

- A new `RingBuffer<f32>` field on `TemporalStateManager` sized for the desired window length (likely 8 to 16 frames at 24 or 30 frames per second), configurable through the existing `TemporalConstraint` metadata structure via a new optional `integration_window_frames: Option<u8>` field.
- Additional aggregate fields — running mean and running variance over the window — that the stabiliser consults alongside the existing `last_global_gain` and `last_frame_luma` state.
- Test fixtures simulating low-frequency luminance drift so the regression coverage demonstrates the new path catches what the single-frame path misses. The existing test patterns in `qdrv-decode::tone_map` give a template for the structure.

**Dependencies and blockers.**

- The window size is the substantive design call. Too short and the buffer adds little over the existing IIR; too long and the controller becomes sluggish during legitimate scene transitions. Exposing `integration_window_frames` as a tunable on `TemporalConstraint` lets operators tune the controller without recompiling, while a documented default of 12 frames gives sensible behaviour out of the box.
- The interaction with the existing `anti_pumping_strength` parameter needs careful documentation so the two controls do not compose unexpectedly. A short design note in `docs/QDRV_TECHNICAL_REFERENCE.md` describing the precedence and combined behaviour is part of the deliverable, not separate from it.

**Estimated complexity.** Small. The data structure is trivial and the workspace already has the lint and test scaffolding to integrate it. The time investment is in tuning the default window size against representative content and in writing fixture-based tests that exercise the low-frequency band specifically.

## 7. AV2 delivery-tier codec support

**Goal.** Add AV2 — the Alliance for Open Media's successor to AV1 — as an alternative delivery-tier codec alongside the current AV1 path, so a `.qdrv32` delivery stream can carry an AV2 bitstream once a production-grade Rust AV2 encoder exists, taking advantage of AV2's improved compression efficiency at equivalent perceptual quality.

**Why this earns this slot.** AV2 is positioned to deliver materially better compression than AV1 at the same visual quality, which maps directly onto QDRV's delivery-tier mandate: smaller files for the same 12-bit 4:4:4 HDR content. It sits at the bottom of the list rather than higher for one decisive reason — the encoder side does not yet exist in a form QDRV can use. The decode ecosystem has moved first: rav2d, a BSD-2-Clause Rust AV2 decoder, is GPL-compatible and could eventually slot in beside the existing dav1d path. But rav1e is AV1-only, and there is no production-grade Rust AV2 encoder. Until that gap closes, QDRV could in principle read AV2 yet not produce it, which inverts the format's purpose. This is a watch-and-track item: progress is gated on an external dependency maturing, not on QDRV design effort.

**Technical surface.**

- A way for the delivery-tier container to signal an AV2 payload distinctly from AV1 — the header already carries a codec byte — so readers reject a codec they cannot decode rather than misparsing it as AV1.
- A `qdrv-codec` encode path mirroring the current `av1` and `temporal` modules once a Rust AV2 encoder is available, holding the same 12-bit 4:4:4, Rec. 2020 primaries, SMPTE ST 2084 transfer contract the AV1 path already enforces.
- A decode path wrapping rav2d behind the same buffer-reuse and error-handling shape as the existing dav1d integration in `qdrv-codec::av1`.
- An assessment of whether the ITU-T T.35 metadata-OBU carriage in `qdrv-codec::metadata_obu` transfers to AV2 unchanged. AV2 inherits AV1's OBU framing, so the carriage is expected to port closely, but the OBU header layout, metadata type, and trailing-marker assumptions must be re-verified against the AV2 bitstream specification rather than assumed.

**Dependencies and blockers.**

- The binding blocker is a production-quality Rust AV2 encoder. rav1e does not encode AV2, and the reference AVM encoder is a C/C++ research codebase rather than a library QDRV would link against under its current pure-Rust-where-possible dependency policy. No substantive encode work begins until a viable Rust encoder exists and clears the same dependency scrutiny already applied to `fpzip-rs` and `zfp-sys-cc`.
- rav2d is early-stage; its decode conformance and API stability need evaluation before it is wired into the verification gates. A decoder that cannot yet round-trip the full feature set QDRV emits is not a dependency QDRV can rely on.
- AV2 support is strictly additive: it must not disturb the AV1 delivery path, which remains the default. The container's codec signalling and reader validation are the mechanism that keeps the two codecs from being confused.

**Estimated complexity.** Currently unscoped, because it is gated on external tooling. Once a Rust AV2 encoder exists, the in-workspace integration mirrors the established AV1 path and is moderate; the real cost sits entirely outside QDRV, in the maturity of the Rust AV2 encode and decode ecosystem. Until that matures, this item stays in watch-and-track status rather than active development.

## 8. AI-assisted dynamic-metadata authoring

**Goal.** Add an optional machine-learning path that analyses a QDRV frame range and proposes dynamic metadata — `OpenDynamicMetadataV2` tone curves and `ObjectMeta` regions — so operators can start from a generated draft instead of authoring every scene and every region by hand. The inference would live in a feature-gated `qdrv-ai` sidecar crate that never becomes a core dependency and never touches the deterministic conversion or render paths, with all model output treated as an advisory suggestion the operator reviews and edits rather than a silent transform.

**Why this earns this slot.** The dynamic-metadata model already exists — `OpenDynamicMetadataV2`, the tone-curve structures, and `ObjectMeta` are all implemented — but today every curve and every region is authored manually, which is the single most labour-intensive part of using QDRV well. A model that drafts that metadata is the one "add AI" idea that fits the format's actual architecture. It sits last for three reasons: it is a convenience layer rather than a format capability; its output cannot participate in the deterministic guarantees the rest of the toolchain makes, so it must be fenced off and strictly opt-in; and, like the AV2 item above, it is gated on an external artefact that does not yet exist — a model trained for QDRV's pixel domain.

**Technical surface.**

- A new feature-gated `qdrv-ai` companion crate that performs inference through `tract` — the pure-Rust ONNX engine from Sonos — rather than the `ort` ONNX Runtime wrapper. `tract` keeps the clean-build, offline-reproducible, and near-zero-`unsafe` properties the workspace depends on; `ort` links the native C++ ONNX Runtime and its FFI surface, which would expand the audit scope, break offline and reproducible builds, and end the workspace's current two-`unsafe`-block guarantee. The pure-Rust path is the design constraint here, not an implementation detail.
- A `qdrv-tool` subcommand, tentatively `qdrv suggest-metadata`, that runs the model over a `.qdrv32` or `.qdrv64` frame range and writes an `OpenDynamicMetadataV2` or `ObjectMeta` JSON document through the same `.part.<pid>` atomic-replace pattern every other QDRV writer uses. The emitted document is explicitly marked as machine-suggested and remains subject to the existing creator-intent-lock semantics, so generated metadata can never silently override authored intent.
- A pixel-domain contract: any model must operate on QDRV's actual data — 12-bit or floating-point HDR in PQ or linear light — not the 8-bit SDR range that off-the-shelf vision models assume. Inputs normalised to `[0.0, 1.0]` over an SDR distribution do not transfer to HDR content and would produce unusable suggestions.

**Dependencies and blockers.**

- The binding blocker is the model itself. No pre-trained network exists for the real task — reading HDR floating-point frames and proposing tone curves and regions — and SDR-domain models do not transfer. This requires either training a model on HDR-domain data or sourcing one, and is the direct analogue of the AV2 item's missing encoder: substantive design effort cannot begin until the external artefact exists.
- Determinism. Inference is not bit-reproducible across hardware, thread counts, or runtime versions, so the feature must stay outside `--deterministic`, outside the fidelity-contract paths, and outside the default build. Its output is a draft for review, never a step in a reproducible transcode.
- Model weights are large binary artefacts that do not belong in the repository or in the deterministic verification gates. The sidecar's tests must exercise the integration with tiny fixture models or by mocking the inference boundary, so the workspace gates stay fast and reproducible whether or not a real model is present.

**Estimated complexity.** Currently unscoped, because — like AV2 — it is gated on external tooling, here a model trained for QDRV's HDR pixel domain. The in-workspace integration is moderate and mirrors patterns the codebase already uses: a feature-gated crate, a single `tract` inference call, and a tool subcommand emitting reviewable JSON. The genuine cost sits outside the workspace, in obtaining and validating a suitable model. Until one exists, this item stays in watch-and-track status rather than active development.

## Contribution scope

Roadmap items are open to external contribution provided the proposal goes through a short design discussion before substantive code is written. The pattern that has worked elsewhere in this workspace is a short design note (one to three paragraphs) covering the metadata shape, the validation rules, and the test fixtures, posted as a draft pull request or as a discussion item, before any implementation lands.

The verification gates are not negotiable: every roadmap item must finish with `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` (both default and `qdrv-codec/zfp` feature configurations), and the full `cargo test --workspace` suite passing cleanly, with regression coverage for any new behaviour written in the same style as the existing tests in the affected crate.
