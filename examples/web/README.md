# QDRV in-browser playback demo

Author: Michael Lauzon <qdrv2026@gmail.com>

This directory is the example page for roadmap item 2 (in-browser playback).
It exercises the [`qdrv-decode-wasm`](../../qdrv-decode-wasm) browser build in
two ways:

- A synthetic 64x64 PQ ramp that proves the wasm/WebGPU tone mapper runs in the
  browser.
- A full browser pipeline that loads a delivery-tier `.qdrv32` file directly,
  or an IVF stream produced with `qdrv mux --format ivf`, recovers one QDRV
  dynamic-metadata document per frame, converts decoded pixels to PQ RGB when
  browser codec support is available, and tone-maps each frame. When direct
  `.qdrv32` AVIF still-image decoding is rejected by the browser, the harness
  renders a labelled metadata preview through the same tone mapper instead of
  failing the example page.

## Build

Build the WebAssembly package from the repository root. The simplest path is
[`wasm-pack`](https://rustwasm.github.io/wasm-pack/):

```bash
wasm-pack build --target web --release qdrv-decode-wasm
```

This writes the JavaScript bindings and `.wasm` binary to
`qdrv-decode-wasm/pkg/`, which the example modules import. That `pkg/`
directory is generated build output and is git-ignored.

If you prefer not to use `wasm-pack`, build with cargo and run `wasm-bindgen`
directly:

```bash
cargo build -p qdrv-decode-wasm --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir qdrv-decode-wasm/pkg \
    target/wasm32-unknown-unknown/release/qdrv_decode_wasm.wasm
```

## Run

ES module imports require an HTTP origin; they do not work from `file://`
(opening `index.html` directly shows CORS/module errors and loads nothing).
The page also imports from the sibling `qdrv-decode-wasm/pkg/` directory, so
you must serve the **repository root** — serving `examples/web/` itself breaks
the import with a `text/html` MIME-type error because the server answers the
module request with a 404 page.

```bash
# from the repository root (NOT from examples/web)
python -m http.server 8080
# Windows, if `python` does not resolve:
py -m http.server 8080
# then browse to:
#   http://localhost:8080/examples/web/index.html
```

For the synthetic path, set a target peak/black luminance and click
**Tone-map synthetic frame**. The canvas shows the tone-mapped PQ signal
displayed as 8-bit for an SDR canvas; an HDR canvas would interpret PQ directly.

For the full playback path, select `test-vectors/ramp-delivery.qdrv32` or
another delivery-tier `.qdrv32` file and click **Decode & tone-map**. IVF remains
available as an alternate input:

```bash
qdrv mux input.qdrv32 out.ivf --format ivf
```

## What this verifies

- `parse_qdrv32_container` validates delivery-tier `.qdrv32` structure and
  returns one metadata record plus one AV1 payload range per frame.
- `wrap_av1_still_as_avif` wraps direct `.qdrv32` AV1 still-picture payloads
  as single-image AVIF data so browser still-image decoders can try them.
- `extract_stream_metadata` recovers per-frame QDRV metadata from ITU-T T.35
  OBUs for IVF/OBU streams.
- `yuv_ncl_to_pq_rgb` bridges decoded 4:4:4 Y'CbCr WebCodecs frames into the
  PQ RGB layout expected by QDRV tone mapping.
- `tone-map.wgsl` runs the per-pixel tone map through WebGPU when available,
  with the wasm `tone_map_frame` path as the fallback.

## Known limitations

- The synthetic tone-mapping demo is functional and is the verified part of
  this page: it exercises the wasm core and the WebGPU shader with the wasm
  CPU path as fallback.
- The full WebCodecs path is **experimental and browser-dependent**. Direct
  `.qdrv32` playback first tries browser AVIF still-image decode for QDRV's
  12-bit 4:4:4 AV1 still-picture payloads, then falls back to a metadata
  preview if the browser rejects that profile. IVF playback requires
  `VideoDecoder` support for AV1 Professional profile, 12-bit 4:4:4; browsers
  limited to 8-bit 4:2:0 AV1 cannot play the IVF stream regardless of the
  page's correctness.
- When the still-image fallback extracts pixels through an RGBA bitmap or
  canvas read, the intermediate is 8-bit and may be colour-managed by the
  browser, so that rendering is approximate rather than the original 12-bit
  signal. The status line names the extraction source (for example
  "AVIF bitmap RGBA") so the fidelity of what is on screen is always visible;
  only the planar Y'CbCr path carries the full-precision signal.
- `qdrv-decode-wasm/pkg/` is generated build output and is not committed to
  the repository; run the build step above before serving the page.
