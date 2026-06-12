# qdrv-decode-wasm

Author: Michael Lauzon <qdrv2026@gmail.com>

WebAssembly bindings for QDRV delivery-tier tone mapping in the browser
(roadmap item 2: in-browser playback of `.qdrv32`).

## Design: codec-free by intent

The native AV1 and mastering codecs (`rav1e`, `dav1d`, `fpzip`, `zfp`) do not
build for `wasm32`, and would dominate the artefact size even if they did. The
browser already ships AV1 decoders through [WebCodecs]: direct `.qdrv32`
still-picture payloads are wrapped as AVIF for `ImageDecoder`, while IVF streams
use `VideoDecoder`. The browser pipeline then hands the decoded delivery-tier
pixels to this crate for QDRV metadata-driven tone mapping.

This crate therefore depends only on the pure-Rust `qdrv-core`, `qdrv-meta`,
`qdrv-decode`, `qdrv-mux` AVIF wrapping, and metadata-OBU parts of `qdrv-codec`,
all of which compile to `wasm32-unknown-unknown`.

The tone-mapping logic lives in the target-independent `tone_map_frame_core`,
which is unit-tested on the host as part of the normal workspace gates. The
`wasm-bindgen` exports are thin wrappers compiled only for `wasm32`, so adding
this crate to the workspace does not pull the wasm toolchain into native
`cargo test` / `clippy` / `fmt` gates.

## API

```text
tone_map_frame(
    pq_rgb: Float32Array,   // interleaved R,G,B per pixel (width*height*3)
    width: number,
    height: number,
    dynamic_json: string,   // per-frame DynamicMeta as JSON
    object_json?: string,   // optional per-frame ObjectMeta (flat + spherical)
    target_max_nits: number,
    target_min_nits: number,
) -> Float32Array           // tone-mapped PQ RGB, same layout/length
```

Supplying `object_json` drives the per-region paths shipped in roadmap item 1:
flat object regions and 360-degree / immersive spherical regions
(equirectangular, cubemap, EAC).

A companion helper bridges the browser's decoded frame to that entry point:

```text
yuv_ncl_to_pq_rgb(
    y: Float32Array,        // luma, normalised to [0, 1]
    cb: Float32Array,       // chroma, normalised to [-0.5, 0.5]
    cr: Float32Array,       // chroma, normalised to [-0.5, 0.5]
    width: number,
    height: number,
) -> Float32Array           // interleaved PQ R,G,B for tone_map_frame
```

It applies the BT.2020 non-constant-luminance inverse matrix so a decoded 4:4:4
Y'CbCr `VideoFrame` can feed `tone_map_frame`. JavaScript de-quantises the
decoder's code values to the normalised ranges above.

The WebGPU path samples the tone curve into a LUT on the CPU so the GPU result
matches the native curve evaluation:

```text
build_tone_curve_lut(
    dynamic_json: string,   // per-frame DynamicMeta as JSON
    size: number,           // number of LUT entries (>= 2)
) -> Float32Array           // curve.evaluate sampled evenly over [0, 1]
```

The direct `.qdrv32` read side parses the QDRV delivery container, validates the
static and per-frame dynamic metadata, and returns byte offsets to each AV1
payload in the original browser `ArrayBuffer`:

```text
parse_qdrv32_container(
    data: Uint8Array,       // complete delivery-tier .qdrv32 file
) -> string                 // JSON manifest: dimensions, metadata, AV1 payload ranges
```

The alternate IVF/OBU read side recovers in-bitstream metadata, using only the
pure-Rust OBU parser from `qdrv-codec`:

```text
extract_stream_metadata(
    stream: Uint8Array,     // AV1 temporal units (e.g. a demuxed IVF stream)
) -> string                 // JSON array of per-frame DynamicMeta
```

## Build

```bash
# native gates (host) build and test the codec-free core:
cargo test -p qdrv-decode-wasm

# wasm build (requires the wasm32 target: rustup target add wasm32-unknown-unknown):
cargo build -p qdrv-decode-wasm --release --target wasm32-unknown-unknown

# browser bindings (requires wasm-pack):
wasm-pack build --target web --release qdrv-decode-wasm
```

A runnable example page lives in [`examples/web/`](../examples/web).

## Browser integration pipeline

1. Load a delivery-tier `.qdrv32` file directly with `parse_qdrv32_container`,
   or load an IVF stream produced by `qdrv mux --format ivf`.
2. Read one per-frame QDRV dynamic metadata document from the `.qdrv32`
   manifest, or from in-bitstream ITU-T T.35 metadata OBUs for IVF/OBU streams.
3. Decode direct `.qdrv32` AV1 still pictures by wrapping each payload with
   `wrap_av1_still_as_avif` and feeding the result to WebCodecs `ImageDecoder`;
   decode IVF temporal units with WebCodecs `VideoDecoder` (12-bit 4:4:4,
   BT.2020 primaries, SMPTE ST 2084 transfer).
4. Convert each decoded Y'CbCr or packed RGB/BGR frame to PQ RGB, then tone-map
   with that frame's metadata and present to an HDR canvas.
5. Run the per-pixel tone map as a WebGPU compute shader
   (`examples/web/tone-map.wgsl`, using `build_tone_curve_lut` for the curve),
   with the wasm `tone_map_frame` as the fallback.

Direct `.qdrv32` parsing is provided by `parse_qdrv32_container`; AVIF wrapping
for direct still-picture decode is provided by `wrap_av1_still_as_avif`;
alternate IVF/OBU metadata extraction is provided by `extract_stream_metadata`;
and steps 4-5 use the YUV bridge in this crate plus the example WebGPU shader.
The browser glue is wired in `examples/web/webcodecs-player.js`, an experimental
browser-runtime path validated in a browser rather than by the native gates.

## Licence

GNU General Public Licence v2.0 (GPLv2).

[WebCodecs]: https://developer.mozilla.org/docs/Web/API/WebCodecs_API
