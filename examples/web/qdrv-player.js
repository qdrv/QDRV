// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Demo glue and shared renderer for qdrv-decode-wasm. Build the wasm package
// and serve this directory over HTTP first; see README.md.
//
// The synthetic demo tone-maps a PQ ramp. The full WebCodecs path in
// `webcodecs-player.js` reuses the exported `toneMapAdaptive` renderer, so both
// paths prefer the WebGPU compute shader and fall back to wasm CPU tone mapping.

import init, {
  tone_map_frame,
  build_tone_curve_lut,
} from "../../qdrv-decode-wasm/pkg/qdrv_decode_wasm.js?v=20260612-qdrv32-preview-fallback";

const WIDTH = 64;
const HEIGHT = 64;
const LUT_SIZE = 1024;
const WEB_HARNESS_BUILD = "20260612-qdrv32-preview-fallback";
const wasmReady = init({
  module_or_path: new URL(
    `../../qdrv-decode-wasm/pkg/qdrv_decode_wasm_bg.wasm?v=${WEB_HARNESS_BUILD}`,
    import.meta.url,
  ),
});

const statusEl = document.getElementById("status");
const runEl = document.getElementById("run");

let dynamicJson = null;
let gpuPromise = null;

function syntheticPqFrame(w, h) {
  const buf = new Float32Array(w * h * 3);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const v = w > 1 ? x / (w - 1) : 0;
      const i = (y * w + x) * 3;
      buf[i] = v;
      buf[i + 1] = v;
      buf[i + 2] = v;
    }
  }
  return buf;
}

function drawPqAs8Bit(canvas, pq, w, h) {
  const ctx = canvas.getContext("2d");
  const img = ctx.createImageData(w, h);
  const clamp = (v) => Math.round(Math.min(1, Math.max(0, v)) * 255);
  for (let p = 0; p < w * h; p++) {
    img.data[p * 4 + 0] = clamp(pq[p * 3 + 0]);
    img.data[p * 4 + 1] = clamp(pq[p * 3 + 1]);
    img.data[p * 4 + 2] = clamp(pq[p * 3 + 2]);
    img.data[p * 4 + 3] = 255;
  }
  ctx.putImageData(img, 0, 0);
}

function refMaxFromMeta(json) {
  try {
    const meta = JSON.parse(json);
    return meta.target_display_hint?.max_luminance_nits ?? 1000.0;
  } catch {
    return 1000.0;
  }
}

async function setupWebGpu() {
  if (!("gpu" in navigator)) return null;
  const adapter = await navigator.gpu.requestAdapter();
  if (!adapter) return null;
  const device = await adapter.requestDevice();
  const wgsl = await (await fetch("./tone-map.wgsl")).text();
  const module = device.createShaderModule({ code: wgsl });
  const pipeline = device.createComputePipeline({
    layout: "auto",
    compute: { module, entryPoint: "main" },
  });
  return { device, pipeline };
}

async function getWebGpu() {
  if (!gpuPromise) {
    gpuPromise = setupWebGpu().catch(() => null);
  }
  return gpuPromise;
}

async function toneMapGpu(g, frame, lut, refMax, minNits, maxNits) {
  const { device, pipeline } = g;
  const pixelCount = frame.length / 3;
  if (!Number.isInteger(pixelCount) || pixelCount > 0xffffffff) {
    throw new Error(`invalid RGB frame length ${frame.length}`);
  }
  const byteLen = frame.byteLength;

  const inputBuf = device.createBuffer({
    size: byteLen,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });
  device.queue.writeBuffer(inputBuf, 0, frame);

  const outputBuf = device.createBuffer({
    size: byteLen,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC,
  });

  const lutBuf = device.createBuffer({
    size: lut.byteLength,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });
  device.queue.writeBuffer(lutBuf, 0, lut);

  const paramsData = new ArrayBuffer(32);
  const dv = new DataView(paramsData);
  dv.setUint32(0, pixelCount, true);
  dv.setUint32(4, lut.length, true);
  dv.setFloat32(8, refMax, true);
  dv.setFloat32(12, minNits, true);
  dv.setFloat32(16, maxNits, true);
  const paramsBuf = device.createBuffer({
    size: 32,
    usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
  });
  device.queue.writeBuffer(paramsBuf, 0, paramsData);

  const bindGroup = device.createBindGroup({
    layout: pipeline.getBindGroupLayout(0),
    entries: [
      { binding: 0, resource: { buffer: inputBuf } },
      { binding: 1, resource: { buffer: outputBuf } },
      { binding: 2, resource: { buffer: lutBuf } },
      { binding: 3, resource: { buffer: paramsBuf } },
    ],
  });

  const readBuf = device.createBuffer({
    size: byteLen,
    usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
  });

  const encoder = device.createCommandEncoder();
  const pass = encoder.beginComputePass();
  pass.setPipeline(pipeline);
  pass.setBindGroup(0, bindGroup);
  pass.dispatchWorkgroups(Math.ceil(pixelCount / 64));
  pass.end();
  encoder.copyBufferToBuffer(outputBuf, 0, readBuf, 0, byteLen);
  device.queue.submit([encoder.finish()]);

  await readBuf.mapAsync(GPUMapMode.READ);
  const out = new Float32Array(readBuf.getMappedRange().slice(0));
  readBuf.unmap();
  return out;
}

export async function toneMapAdaptive(frame, width, height, dynamicJson, maxNits, minNits) {
  await wasmReady;
  const gpu = await getWebGpu();
  if (gpu) {
    const lut = build_tone_curve_lut(dynamicJson, LUT_SIZE);
    const refMax = refMaxFromMeta(dynamicJson);
    return {
      pixels: await toneMapGpu(gpu, frame, lut, refMax, minNits, maxNits),
      path: "WebGPU compute shader",
    };
  }
  return {
    pixels: tone_map_frame(
      frame,
      width,
      height,
      dynamicJson,
      undefined,
      maxNits,
      minNits,
    ),
    path: "wasm CPU fallback",
  };
}

async function main() {
  await wasmReady;
  dynamicJson = await (await fetch("./sample-dynamic.json")).text();
  const gpu = await getWebGpu();

  runEl.disabled = false;
  statusEl.textContent = gpu
    ? "Ready - WebGPU available."
    : "Ready - WebGPU unavailable; using the wasm fallback.";

  runEl.addEventListener("click", async () => {
    try {
      const maxNits = parseFloat(document.getElementById("maxNits").value);
      const minNits = parseFloat(document.getElementById("minNits").value);
      const frame = syntheticPqFrame(WIDTH, HEIGHT);
      const { pixels: out, path } = await toneMapAdaptive(
        frame,
        WIDTH,
        HEIGHT,
        dynamicJson,
        maxNits,
        minNits,
      );
      drawPqAs8Bit(document.getElementById("out"), out, WIDTH, HEIGHT);
      statusEl.textContent = `Tone-mapped ${WIDTH}x${HEIGHT} to ${maxNits} nits via ${path}.`;
    } catch (e) {
      statusEl.textContent = "Error: " + (e.message ?? e);
    }
  });
}

main().catch((e) => {
  statusEl.textContent = "Failed to initialise: " + (e.message ?? e);
});
