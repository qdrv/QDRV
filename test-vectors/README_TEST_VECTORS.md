# QDRV Checked-in Test Vectors

Author: Michael Lauzon <qdrv2026@gmail.com>

This directory contains deterministic binary test artefacts used by automated integration tests in `qdrv-io`. The vectors are intentionally small and stable so that parser, writer, and transcode regressions are detected quickly in CI and local development.

## Purpose of checked-in vectors

The vectors are checked into source control to provide a fixed binary baseline that does not depend on runtime generation during test execution. This allows the test suite to:

- detect unexpected byte-level changes via SHA-256 checks;
- validate that reader behaviour remains compatible with known-good files;
- confirm parse and round-trip behaviour for both delivery and mastering tiers.

Because these files are treated as deterministic fixtures, their hashes are part of the test contract.

## Vector inventory and semantics

### `ramp-delivery.qdrv32`

- Tier: delivery (`.qdrv32`)
- Dimensions and frames: `16x4`, `1` frame
- Pixel pattern: neutral-grey horizontal ramp from approximately `0` nits to approximately `1000` nits after mastering-to-delivery transcode
- Encoding path: AV1 delivery encode (`quantizer 0`, `speed 7`, `container-version v1`, single-thread deterministic path in generator)
- File size: `1,598` bytes
- SHA-256: `2a17a0333260c93476111f162ca8f1e72fc22d745f4cb3bd33e47c3fae548c79`
- Test intent: parser geometry/tier checks, expected endpoint level checks (with delivery tolerance), and parse-write-parse round-trip coverage

### `ramp-mastering.qdrv64`

- Tier: mastering (`.qdrv64`)
- Dimensions and frames: `16x4`, `1` frame
- Pixel pattern: linear-light neutral-grey horizontal ramp from `0.0` to `1000.0` nits
- Encoding path: lossless mastering encode with `fpzip` and `container-version v1`
- File size: `2,432` bytes
- SHA-256: `0ea98a2e05db07427c9189b30281d76c20ff87670d3c768785ffd7e99e697498`
- Test intent: parser geometry/tier checks, exact endpoint checks in mastering space, and parse-write-parse round-trip coverage

## Determinism expectations

These vectors are expected to be deterministic when generated with the documented commands and current code paths:

- `ramp-delivery.qdrv32` determinism depends on fixed generator settings (`--quantizer 0`, `--speed 7`, `--container-version v1`) and the generator's single-thread encode configuration.
- `ramp-mastering.qdrv64` determinism depends on using `--mastering --mastering-codec fpzip --container-version v1` with unchanged mastering writer behaviour.
- The round-trip test path uses separate in-memory writer settings for delivery (`speed: 10`, `quantizer: 0`, `threads: 1`) and validates parse/metadata/pixel behaviour rather than fixture byte identity.
- The integration test `checked_in_vectors_have_expected_sha256` treats both hashes as canonical.

Any hash drift must be treated as intentional format/behaviour change or as a regression to investigate.

## Regeneration commands and assumptions

Run from repository root:

```bash
cargo run -p qdrv-tool -- write-test test-vectors/ramp-delivery.qdrv32 --width 16 --height 4 --frames 1 --quantizer 0 --speed 7 --container-version v1
cargo run -p qdrv-tool -- write-test test-vectors/ramp-mastering.qdrv64 --width 16 --height 4 --frames 1 --mastering --mastering-codec fpzip --container-version v1
```

Assumptions:

- commands are run from a workspace with the current repository sources;
- output paths refer to files in `test-vectors/`;
- no manual editing of generated binary files occurs after generation.

## Verification process (hash + parser + round-trip)

Primary validation command:

```bash
cargo test -p qdrv-io --test checked_in_vectors
```

This test target validates all of the following:

- SHA-256 stability for `ramp-delivery.qdrv32` and `ramp-mastering.qdrv64`;
- parser correctness for tier, width/height, frame count, and pixel count;
- expected ramp levels:
  - delivery vector endpoints validated with the test tolerance (`DELIVERY_TOLERANCE`);
  - mastering vector endpoints validated as exact `0.0` and `1000.0` values;
- parse-write-parse round-trip for both tiers:
  - delivery round-trip through `write_delivery_file` with `speed: 10`, `quantizer: 0`, `threads: 1`, `chroma: Cs444`;
  - mastering round-trip through `write_mastering_file` with `MasteringCodec::Fpzip`;
- static metadata and per-frame dynamic metadata preservation checks.

## Update policy for vectors

Update checked-in vectors only when a deliberate change affects deterministic output (for example, format evolution, metadata contract updates, or intended codec/encoder behaviour changes).

When updating vectors:

1. Regenerate both files using the exact commands above.
2. Recompute and confirm file sizes and SHA-256 values.
3. Update expected hashes in `qdrv-io/tests/checked_in_vectors.rs`.
4. Update this README to reflect new hashes, sizes, or semantic expectations.
5. Run `cargo test -p qdrv-io --test checked_in_vectors` and ensure it passes.
6. Document the reason for vector changes in the associated change request.

Avoid opportunistic vector refreshes without a clear rationale, as unnecessary churn weakens regression signal quality.

## Troubleshooting mismatch cases

### Hash mismatch, parser tests still pass

Likely deterministic output changed (intentionally or accidentally). Regenerate using the exact commands in this README and compare results. If the change is intended, update hashes and documentation together; if not, treat as a regression.

### Delivery endpoint/tolerance assertion failures

Confirm the delivery vector was generated with `--quantizer 0 --speed 7 --container-version v1` and that no local code changes altered the delivery encode path used by `write-test`.

### Mastering endpoint assertion failures

Confirm generation used `--mastering --mastering-codec fpzip --container-version v1` and that mastering writer/reader code has not introduced unexpected transforms.

### Parse failures or missing file errors

Regenerate vectors with the documented commands and re-run the integration test. Ensure file names and paths match exactly:

- `test-vectors/ramp-delivery.qdrv32`
- `test-vectors/ramp-mastering.qdrv64`

### Round-trip assertion failures

Investigate writer/reader behavioural changes first (delivery AV1 path or mastering lossless path), then validate metadata and pixel-space assumptions in `qdrv-io/tests/checked_in_vectors.rs`.

## Licence

These test vectors are released under the GNU General Public Licence v2.0 (GPLv2).
