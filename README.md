# QDRV — Quantum Dynamic Range Video

Author: Michael Lauzon <qdrv2026@gmail.com>

**Version:** 0.1.0 (Working Draft)  
**Licence:** GNU General Public Licence v2.0 (GPLv2)  
**Language:** Rust (`edition = "2024"`, `rust-version = "1.95.0"`)

QDRV (Quantum Dynamic Range Video) is an open, floating-point dynamic-range video format and toolchain — designed as a successor to integer HDR (HDR10, HDR10+) and to proprietary Dolby Vision packaging. It has two operational tiers:

- `.qdrv64` mastering tier (Float64 linear-light RGB)
- `.qdrv32` delivery tier (Float32 PQ RGB at API boundary, AV1-compressed payloads)

QDRV itself is specified and implemented here as a fully open, royalty-free format and tooling stack. Unlike branded HDR10-family ecosystem variants and Dolby Vision packaging programmes, which may impose separate trademark, certification, or licensing requirements, the core QDRV format does not require proprietary programme participation.

QDRV exists to address a recurring gap in floating-point video mastering and delivery work: teams often need a format that is mathematically transparent enough for mastering and validation, while still producing delivery artefacts that can interoperate with practical downstream systems. Existing HDR pipelines — including HDR10, HDR10+, and Dolby Vision packaging — force an early compromise between precision, observability, and interchange because they quantise to integer code values before the colourist has finished decision-making. QDRV is intended to keep precision and observability concerns explicit at every stage, rather than hiding them behind opaque integer-domain processing assumptions.

The project follows an implementation-first documentation style. In practical terms, that means this README and companion docs describe behaviour that is currently implemented, tested, and validated in this repository, not aspirational roadmap claims. This is deliberate, because predictability and auditability are more useful than broad promises when teams are integrating format tooling into CI, verification, or production pre-flight checks.

## Table of Contents

- [Overview and Goals](#overview-and-goals)
- [Key Features and Capabilities](#key-features-and-capabilities)
- [Architecture and Crate Map](#architecture-and-crate-map)
- [Format and Version Compatibility Matrix](#format-and-version-compatibility-matrix)
- [Installation and Prerequisites](#installation-and-prerequisites)
- [CLI Usage](#cli-usage)
- [Conformance and Test Vectors](#conformance-and-test-vectors)
- [Interoperability, DV Adapter Boundary, and Loss Reporting](#interoperability-dv-adapter-boundary-and-loss-reporting)
- [Deterministic Mode and Fidelity Contracts](#deterministic-mode-and-fidelity-contracts)
- [Performance Notes](#performance-notes)
- [Limitations and Non-Goals](#limitations-and-non-goals)
- [Troubleshooting](#troubleshooting)
- [Development Workflow](#development-workflow)
- [Docs Map](#docs-map)
- [Licence](#licence)

## Overview and Goals

QDRV aims to provide a practical, open floating-point video workflow built on public standards (Rec. 2100 / Rec. 2020, SMPTE ST 2084, SMPTE ST 2094 framework) while preserving implementation clarity and deterministic validation paths. The immediate problem space is not only "store dynamic-range frames at higher precision than integer HDR formats allow", but also "make conversion, metadata decisions, and quality gates inspectable enough that engineers can reason about them under real release pressure".

The two-tier model reflects that split of responsibilities. `.qdrv64` is the high-precision mastering tier, where preserving intent and analytical headroom is more important than bit-rate efficiency. `.qdrv32` is the delivery-facing tier, where the data contract becomes PQ-at-boundary plus compressed payloads suitable for practical exchange and downstream processing. Keeping both tiers first-class in one toolchain reduces hidden format jumps and simplifies traceability between source and delivery outputs.

The design philosophy is conservative and explicit: when there is a trade-off between permissive behaviour and predictable behaviour, QDRV generally chooses predictable behaviour and surfaces why. This is visible in strict version checks, schema enforcement, deterministic options, and machine-readable loss reporting. The cost of that choice is that some workflows are intentionally less permissive than ad hoc scripts, but the benefit is lower ambiguity in CI and operator tooling.

Current goals in this repository:

1. Preserve high-precision mastering data in `.qdrv64`.
2. Produce interoperable delivery content in `.qdrv32`.
3. Keep read/write and metadata compatibility rules explicit.
4. Support reproducible engineering workflows (conformance, manifest signing, fidelity contracts, interop reporting).

## Key Features and Capabilities

The feature set is organised around operational reliability rather than one-off conversions. Most commands and APIs are designed to be scriptable, machine-checkable, and clear about degradation boundaries, because those qualities matter most when integrating floating-point video processing into automated pipelines.

Another guiding choice is to keep metadata and container evolution visible instead of implicit. That is why compatibility rules are encoded directly in read/write paths and schema validation, and why interop export includes explicit loss semantics. This can feel stricter than "best effort" tools, but it gives teams a dependable baseline for reproducibility.

- Two-tier floating-point video pipeline (`.qdrv64` mastering, `.qdrv32` delivery).
- Container compatibility: read v1/v2, default write v2, optional compatibility write v1.
- Metadata schema compatibility enforcement, including Open Dynamic Metadata v2.
- Streaming reader (`QdrvStreamReader`) for bounded, frame-by-frame processing.
- Deterministic conversion mode and contract-based fidelity checks.
- Conformance corpus generation and execution tooling.
- Interoperability exporters with explicit, machine-readable loss reporting.
- AV1 decode state/buffer reuse and MP4 mux hardening for large payload scenarios.

## Architecture and Crate Map

QDRV is split into focused crates so that colour science, metadata policy, container IO, codecs, and operator tooling evolve with clearer boundaries. The intent is architectural clarity rather than abstraction for its own sake: each crate corresponds to a distinct concern that frequently changes at a different pace in real projects.

This separation has practical trade-offs. It improves auditability and test targeting (for example, validating schema logic independently from codec behaviour), but it also requires stronger API contracts across crate boundaries and slightly more coordination when changing cross-cutting behaviour. The repository chooses that trade-off because explicit dependency boundaries reduce accidental coupling over time.

High-level flow:

1. Mastering ingest or generation (`.qdrv64`)
2. Optional conversion (`qdrv convert`) to delivery (`.qdrv32`)
3. Inspect / export interop / conformance / manifest workflows

Workspace crates:

| Crate | Responsibility |
|---|---|
| `qdrv-core` | Pixel types, PQ maths, colour transforms, fidelity metrics |
| `qdrv-meta` | Static/dynamic metadata, compatibility checks, v2 structures, manifests, interop models |
| `qdrv-encode` | Mastering-to-delivery transcode path and encode options |
| `qdrv-decode` | Tone mapping, v2 policy application, temporal anti-pumping, SDR fallback |
| `qdrv-codec` | AV1 encode/decode (`rav1e` + `dav1d`), mastering compression (`fpzip`, optional `zfp`) |
| `qdrv-io` | Container reader/writer, version enforcement, streaming read path, size bounds checks |
| `qdrv-mux` | AV1-in-MP4 muxing with `stco`/`co64`, `mdat largesize`, and HDR `colr nclx` signalling |
| `qdrv-tool` | CLI entrypoint and operator-facing workflows |

The repository also ships a `test-vectors/` data directory holding the
checked-in deterministic vector corpus and its expected SHA-256 hashes
(consumed by `qdrv-io`'s integration tests). It is not a Cargo crate.

The crate map also reflects an interoperability strategy: `qdrv-meta` defines and validates metadata semantics, `qdrv-io` enforces container contracts, and `qdrv-tool` exposes those decisions through operator-visible commands. That layering helps ensure that policy and compatibility decisions are not hidden in CLI glue code alone.

## Format and Version Compatibility Matrix

Container and schema evolution is treated as a compatibility management problem, not just a file-format problem. QDRV currently supports reading both v1 and v2 containers, writes v2 by default, and permits explicit v1 output for compatibility workflows. This lets teams move forward incrementally while still serving legacy consumers where required.

The v1/v2 behaviour is intentionally strict at boundaries. Old or unknown versions are rejected rather than guessed, and future versions are rejected rather than interpreted optimistically. That decision favours deterministic failure modes: operators can identify mismatch causes early, instead of accepting silent behaviour drift that is difficult to diagnose later.

### Container compatibility

| Operation | Current behaviour |
|---|---|
| Read container v1 | Supported |
| Read container v2 | Supported |
| Default write | Container v2 |
| Compatibility write | Container v1 via `--container-version v1` |
| Unsupported old/unknown versions | Rejected with unsupported-version error |
| Future versions | Rejected with future-version error |

### Metadata schema compatibility

Schema handling follows the same approach. v2 containers support both metadata schema v1 and v2 to provide a transition path, while v1 containers remain locked to schema v1 to avoid ambiguous interpretation. This distinction keeps compatibility behaviour explicit for integrators who need predictable migration planning.

| Container version | Supported metadata schema versions | Notes |
|---|---|---|
| v1 | v1 only | v1 + schema v2 is rejected |
| v2 | v1 or v2 | Transition-friendly path |

Additional metadata rules enforced:

- Static/dynamic `metadata_schema_version` values must match.
- Schema v2 (`METADATA_SCHEMA_V2`) must include `open_dynamic_v2`.
- Schema v1 must not include `open_dynamic_v2`.

Taken together, these rules form a practical evolution contract: you can migrate format versions deliberately, but combinations that would blur semantic meaning are rejected early. That makes compatibility failures louder, but it substantially reduces ambiguity during long-lived deployment transitions.

## Installation and Prerequisites

The build requirements are intentionally conventional for Rust projects with native codec dependencies. Most installation issues come from system package discovery (`pkg-config` and `dav1d`) rather than Rust itself, so validating native toolchain visibility early is usually the fastest path to a clean setup.

### Required toolchain

- Rust stable `1.95.0` or newer
- Cargo (bundled with Rust)
- C toolchain suitable for native crates
- `nasm` (recommended for AV1 build performance)
- `dav1d` development libraries discoverable by `pkg-config`

### Platform notes

#### Windows

Recommended path is MSVC Rust toolchain plus MSYS2 packages for `dav1d` and `pkg-config`.

Typical requirements:

- Visual Studio C++ Build Tools (or full Visual Studio with C++ workload)
- MSYS2 UCRT toolchain packages (for example `dav1d`, `pkgconf`, `nasm`)
- `pkg-config` available in `PATH`
- If detection fails, set `PKG_CONFIG_PATH` to the folder containing `dav1d.pc`

If your environment already resolves `dav1d` through `pkg-config`, no additional configuration is needed.

#### Linux

Install Rust, `pkg-config`, `nasm`, and `libdav1d-dev` (package names vary by distro).

#### macOS

Install Rust plus `nasm`, `pkg-config`, and `dav1d` (for example via Homebrew).

### Build and install

```bash
# Build all crates
cargo build --workspace

# Install CLI binary (`qdrv`)
cargo install --path qdrv-tool
```

Optional: enable ZFP mastering compression support.

```bash
cargo build --workspace --features zfp
```

## CLI Usage

The CLI is designed as an operator surface over crate-level capabilities: inspection, conversion, metadata export, interop analysis, manifest handling, and conformance execution. Commands are structured so they can be used interactively during debugging and non-interactively in CI pipelines.

Where possible, command outputs are shaped for downstream automation (for example JSON reports and deterministic vector workflows) rather than only human-readable logs. That design helps teams move from exploratory usage to repeatable validation without rewriting tooling around the CLI.

The `qdrv` binary currently exposes:

- `info`
- `pq {--nits <NITS> | --pq <PQ>}`
- `meta-static`
- `meta-dynamic`
- `meta-dynamic-v2`
- `write-test <output>`
- `convert <input> <output>`
- `hdr10plus <input> <output>`
- `inspect <file>`
- `mux <input.qdrv32> <output.mp4>`
- `export-interop <input> <output_dir>`
- `manifest-sign <input> <output> {--key <key> | --key-file <path>} [--signer <signer>]`
- `manifest-verify <input> <manifest> {--key <key> | --key-file <path>}`
- `conformance-generate-open <output_dir> {--key <key> | --key-file <path> | --allow-public-default-key}`
- `conformance-run <manifest> <output_dir> {--key <key> | --key-file <path>}`

### Signing key handling

Manifest and conformance commands accept the signing key from three mutually
exclusive sources, listed in precedence-friendly order:

1. **`QDRV_SIGNING_KEY` environment variable** — preferred for CI/automation;
   never appears in process listings or shell history.
2. **`--key-file <path>`** — preferred for production use with on-disk key
   files; reads raw bytes (no UTF-8 requirement) and strips one trailing
   `\r?\n`.
3. **`--key <value>`** — convenient for ad-hoc local use; **avoid in shared
   shells, CI logs, or anywhere `ps`/history could capture the argument**.

`conformance-generate-open` can sign with a built-in public default key
so the open-vectors workflow is reproducible across machines, but the
default key path is **fail-closed**: it only fires when the operator
explicitly passes `--allow-public-default-key` *and* no `--key`,
`QDRV_SIGNING_KEY`, or `--key-file` value is supplied. Without the
opt-in flag the command fails with a message listing all four ways to
supply a key. Empty values from any source (`--key ""`,
`QDRV_SIGNING_KEY=""`) are treated as unset for the same fail-closed
check. All other commands require one of the three explicit sources.

**Important:** signatures produced with the built-in default key are
**not** authenticity evidence. They exist so the open-vectors corpus
reproduces byte-for-byte across machines that have not been configured
with a private key. Production callers must supply their own key via
`QDRV_SIGNING_KEY` or `--key-file`.

`hdr10plus` mode options: `--mode basic` (default), `--mode advanced`, `--mode adaptive`, `--mode gaming` (legacy `--advanced` remains supported).

Representative commands:

```bash
# Inspect format summary
qdrv info

# Generate test files
qdrv write-test sample.qdrv32 --width 256 --height 64 --frames 1
qdrv write-test sample.qdrv64 --mastering --mastering-codec fpzip
qdrv write-test sample-v1.qdrv32 --container-version v1

# Mastering -> delivery conversion
qdrv convert master.qdrv64 delivery.qdrv32 --quantizer 40 --speed 6

# Export HDR10+ profile metadata (basic/advanced/adaptive/gaming)
qdrv hdr10plus delivery.qdrv32 hdr10plus-basic.json --mode basic
qdrv hdr10plus delivery.qdrv32 hdr10plus-adaptive.json --mode adaptive
qdrv hdr10plus delivery.qdrv32 hdr10plus-gaming.json --mode gaming

# Deterministic conversion + metadata v2 policy tags
qdrv convert master.qdrv64 delivery.qdrv32 --deterministic --metadata-v2 --ambient-lux 120 --display-model oled --frame-time-ms 8.3

# Compatibility output in v1 container
qdrv convert master.qdrv64 delivery-v1.qdrv32 --container-version v1

# Export HDR10 / HDR10+ / DV-compatible interop bundle
qdrv export-interop delivery.qdrv32 out/

# Mux a delivery-tier .qdrv32 into a standards-compliant .mp4 (AV1 + HDR `colr nclx`)
qdrv mux delivery.qdrv32 delivery.mp4 --frame-rate 24 --quantizer 40 --speed 6 --keyframe-interval 120

# Manifest and conformance workflow
# Preferred (no secret in argv): set the key via env var or read from a file.
QDRV_SIGNING_KEY=demo-key qdrv manifest-sign meta.json meta.manifest.json --signer qdrv-tool
qdrv manifest-sign meta.json meta.manifest.json --key-file /etc/qdrv/signing.key --signer qdrv-tool
# `--key VALUE` still works for ad-hoc local use, but the value will appear in `ps` and shell history:
qdrv manifest-verify meta.json meta.manifest.json --key demo-key

# Conformance generate: production callers should supply their own key.
qdrv conformance-generate-open conformance/ --key-file /etc/qdrv/signing.key
# Open-vectors reproducible run uses the built-in public default key
# (signatures are NOT authenticity evidence; opt-in is required):
qdrv conformance-generate-open conformance/ --allow-public-default-key
qdrv conformance-run conformance/conformance-manifest.json conformance-results/ --key-file /etc/qdrv/signing.key
```

## Conformance and Test Vectors

Conformance support exists to answer a practical question: can the same content and pipeline produce repeatable outcomes across time, environments, and dependency updates? Checked-in vectors provide a stable baseline that can be validated in routine CI runs before teams spend effort on deeper investigation.

The deterministic corpus and manifest-driven workflows are intentionally complementary. Fixed vectors are useful for fast regression detection, while generated conformance corpora are useful for broader scenario coverage when preparing releases or validating environmental changes.

Checked-in deterministic vectors live in `test-vectors/`:

- `ramp-delivery.qdrv32`  
  `SHA-256: 2a17a0333260c93476111f162ca8f1e72fc22d745f4cb3bd33e47c3fae548c79`
- `ramp-mastering.qdrv64`  
  `SHA-256: 0ea98a2e05db07427c9189b30281d76c20ff87670d3c768785ffd7e99e697498`

Core validation commands:

```bash
# Static/build checks
cargo check --workspace

# Full workspace tests
cargo test --workspace

# Checked-in vector validation
cargo test -p qdrv-io --test checked_in_vectors

# Optional fresh conformance corpus (opt-in to the built-in public key
# explicitly; production callers would pass --key-file instead).
qdrv conformance-generate-open scratch/conformance --allow-public-default-key
qdrv conformance-run scratch/conformance/conformance-manifest.json scratch/conformance-out \
    --key qdrv-open-conformance-key
# In production, prefer one of:
#   QDRV_SIGNING_KEY=… qdrv conformance-run …
#   qdrv conformance-run … --key-file /path/to/signing.key
```

In production-oriented workflows, a common pattern is to run checked-in vector validation on every change, then run broader conformance jobs at release gates or dependency bump checkpoints. This balances turnaround time with confidence depth.

## Interoperability, DV Adapter Boundary, and Loss Reporting

Interoperability in QDRV is designed to be explicit about what is preserved, transformed, approximated, or dropped when targeting downstream formats and ecosystems. Rather than presenting interop as a binary success/failure state, QDRV exports structured reports so operators can decide whether the resulting trade-offs are acceptable for their delivery context.

This boundary is particularly important around Dolby Vision-adjacent workflows. The repository provides open, inspectable export paths and reporting, but it does not claim certified proprietary packaging in open code. Keeping this boundary explicit avoids accidental assumptions in production pipelines that require certification-grade artefacts.

`qdrv export-interop` emits:

- HDR10 raw payload (`RGB10LE`)
- HDR10+ profile JSON (`mode=basic` in interop bundle)
- Open DV-compatible sidecar JSON
- Combined loss report JSON
- DV adapter report JSON

The open exporter intentionally reports dropped, approximated, and unsupported fields per target. This is a design feature, not a warning-only path.

`qdrv hdr10plus` exports profile-aware JSON for `basic`, `advanced`, `adaptive`, and `gaming` modes. Every export includes a machine-readable compatibility report with strict `certification_status: not_certified` markers and missing certification capabilities.

Certified Dolby Vision packaging is out of scope for open code in this repository. Optional integration is provided via `--dv-tool-cmd` placeholders:

- `{sidecar}`
- `{rpu}`
- `{report}`

In practical production use, this means teams can integrate open interop exports as pre-flight artefacts, then hand off to proprietary toolchains where certification or closed packaging is required. The reporting layer is intended to make that hand-off auditable instead of implicit.

## Deterministic Mode and Fidelity Contracts

Determinism and fidelity gates exist because many dynamic-range regressions are subtle: they can pass visual spot checks yet fail quality or compatibility expectations over time. QDRV therefore supports deterministic conversion controls and contract-based thresholds so teams can encode quality intent directly into automation.

Deterministic mode is most useful when you need stable comparisons across runs (for example CI, reproducibility investigations, or release sign-off baselines). Fidelity contracts are most useful when you need objective pass/fail criteria rather than manual judgement alone.

Deterministic conversion:

- `qdrv convert --deterministic` enables stable processing choices (including deterministic AV1 threading configuration and deterministic encode path behaviour).

Fidelity contract enforcement:

- `qdrv convert --fidelity-contract <path>` enables threshold-based gating.
- Supported contract metrics include `psnr_db_min`, `ssim_min`, `delta_e_max`, and optional `vmaf_hdr_min`.

VMAF-HDR backend resolution order:

1. External command template via `QDRV_VMAF_HDR_CMD`
2. `ffmpeg`/`libvmaf` autodetection path
3. Deterministic approximation fallback

When enabling `vmaf_hdr_min`, treat backend selection as an explicit operational decision. If your release process requires high-fidelity scoring from a specific toolchain, configure and validate that backend explicitly.

**Surrogate-acceptance opt-in.** When neither a `QDRV_VMAF_HDR_CMD` template nor a working `ffmpeg`/`libvmaf` is available, QDRV can score frames with a deterministic in-repo approximation. To prevent a `vmaf_hdr_min` contract gate from silently passing on the surrogate, the approximation is **fail-closed by default**: the score is withheld and the contract evaluator reports "metric unavailable", failing the gate. Operators who accept the surrogate explicitly (for example, when running on synthetic fixtures below the libvmaf 33-pixel minimum) must opt in by setting `QDRV_VMAF_HDR_ALLOW_APPROX=1` (or `true`). The acceptance note recorded with each measurement makes the opt-in visible in the `fidelity_notes` of the conformance summary.

## Performance Notes

Performance work in QDRV focuses on predictable memory behaviour and safer large-file handling in addition to raw throughput. In practical terms, this means favouring stream-oriented reads, buffer reuse patterns, and allocation guards in hotspots that are common in inspection, conformance, and interop tasks.

- `QdrvStreamReader` avoids mandatory full-file materialisation in inspect/conformance/interop paths.
- AV1 decode path reuses `Av1Decoder` state and scratch buffers to reduce allocation churn.
- `upsample_420_into` supports caller-managed output buffers for reuse-heavy workflows.
- Reader and writer enforce strict size bounds before allocation (`metadata`, frame payloads, frame area, frame count).
- MP4 mux path handles large file offsets correctly via `stco`/`co64` and `mdat largesize` logic.

These optimizations are intentionally pragmatic and implementation-oriented. They are meant to reduce avoidable overhead and failure risk in real workloads, not to imply that every pipeline stage is globally optimized for every hardware profile.

## Limitations and Non-Goals

QDRV intentionally scopes itself to open, inspectable functionality and explicit compatibility behaviour. Some constraints are technical, while others are governance choices about what can be implemented and validated in an open repository. Being explicit about these limits helps prevent incorrect production assumptions.

Several non-goals also protect implementation clarity. For example, keeping conversion input restricted to mastering-tier data and keeping metadata v2 authoring policy-driven both reduce ambiguous behaviour surfaces. This can feel narrower than general-purpose media tooling, but it makes format and quality contracts easier to reason about and test.

- Open code does not generate certified Dolby Vision bitstreams.
- `qdrv convert` currently accepts mastering input only (`.qdrv64`).
- Metadata v2 CLI authoring is policy-driven (`--metadata-v2` plus policy flags), not a full arbitrary scene/object editor.
- Mastering-tier (`.qdrv64`) streams **cannot carry delivery-side v2 adaptation policy** —
  the writer rejects `DynamicMeta.inverse_tone_mapping_hint`,
  `OpenDynamicMetadataV2.adaptation_layer`, `.ambient_policy`,
  `.gaming_profile`, and `.inverse_tone_mapping_hint` on mastering files.
  Creative-intent v2 fields (scene/object constraints, temporal controls,
  local tone-map grid) remain allowed on both tiers; only adaptation
  fields are gated. Mastering files written before this rule was added
  that happened to carry these fields will fail to load with
  `IoError::InvalidMetadata`; regenerate them from source if encountered.
- ZFP mastering compression is optional and feature-gated (`--features zfp`).
- High-fidelity VMAF-HDR scoring may require external tooling; deterministic fallback remains available.
- Raw codec mode is intended for tests/diagnostics, not production interchange.

## Troubleshooting

Most setup and usage issues resolve quickly once dependency visibility and compatibility rules are checked explicitly. The list below highlights the most common failure modes and the expected corrective path.

- `cannot find library dav1d` or `pkg-config` errors:
  - Ensure `dav1d` development files are installed.
  - Ensure `pkg-config` is installed and in `PATH`.
  - On Windows, ensure your `pkg-config` environment can resolve `dav1d.pc` (set `PKG_CONFIG_PATH` if needed).
- `--container-version v1` with `--metadata-v2` fails:
  - Expected behaviour; container v1 requires metadata schema v1.
- `qdrv convert` rejects input as non-mastering:
  - Expected behaviour; input must be a mastering-tier `.qdrv64` file.
- Interop output does not include proprietary DV artefacts:
  - Expected without an external `--dv-tool-cmd` adapter.
- Fidelity contract `vmaf_hdr_min` backend notes mention fallback:
  - Install/configure external backend tools if you need non-fallback scoring.

## Development Workflow

A practical local routine is to run quick structural checks first, then full tests, then vector validation, and finally optional conformance generation/runs when broader coverage is needed. This order usually gives fast feedback for everyday changes while still supporting deeper release-oriented validation.

Suggested routine from repository root:

```bash
# 1) Build quickly
cargo check --workspace

# 2) Format and lint (workspace lint policy is enforced as hard errors)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features qdrv-codec/zfp -- -D warnings

# 3) Run tests
cargo test --workspace
cargo test --workspace --features qdrv-codec/zfp

# 4) Validate checked-in vectors
cargo test -p qdrv-io --test checked_in_vectors

# 5) Optional conformance run (opt-in to the built-in public key)
qdrv conformance-generate-open scratch/conformance --allow-public-default-key
qdrv conformance-run scratch/conformance/conformance-manifest.json scratch/conformance-out --key qdrv-open-conformance-key
```

The workspace lint policy is defined under `[workspace.lints]` in the root `Cargo.toml` and is enforced by every member crate via `[lints]\nworkspace = true`. Run `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` (and the same with `--features qdrv-codec/zfp`) before publishing changes; both must finish warning-free for the project's verification gates to remain green.

Common local tasks:

- Use `qdrv write-test` to generate deterministic sample fixtures.
- Use `qdrv inspect` to inspect static and per-frame metadata.
- Use `qdrv export-interop` to validate downstream conversion and loss-report surfaces.

## Docs Map

The documents below provide deeper implementation and operational detail than this README. They are useful when you need precise format semantics, container layout, or operational behaviour references.

Primary project documentation:

- [`docs/QDRV_SPEC.md`](docs/QDRV_SPEC.md) — implementation-aligned specification profile
- [`docs/QDRV_TECHNICAL_REFERENCE.md`](docs/QDRV_TECHNICAL_REFERENCE.md) — implementation details and operational behaviour
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — proposed future work, ordered by expected operator impact
- [`test-vectors/README_TEST_VECTORS.md`](test-vectors/README_TEST_VECTORS.md) — checked-in corpus details and regeneration commands

## Licence

GNU General Public Licence v2.0 (GPLv2).
