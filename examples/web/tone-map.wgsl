// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//
// WebGPU compute-shader port of the QDRV per-pixel global tone map. It mirrors
// `qdrv-decode::tone_map`'s per-channel pipeline: PQ EOTF -> normalise by the
// reference peak -> evaluate the tone curve -> scale to the target peak ->
// PQ OETF. The (monotone-cubic or linear) curve is evaluated on the CPU into
// `lut` via `qdrv_decode_wasm::build_tone_curve_lut`, so the GPU result matches
// the native curve evaluation; this shader only samples that LUT.

struct Params {
  pixel_count : u32,
  lut_size : u32,
  ref_max : f32,
  target_min : f32,
  target_max : f32,
};

@group(0) @binding(0) var<storage, read> input : array<f32>;        // interleaved R,G,B
@group(0) @binding(1) var<storage, read_write> output : array<f32>; // interleaved R,G,B
@group(0) @binding(2) var<storage, read> lut : array<f32>;
@group(0) @binding(3) var<uniform> params : Params;

// SMPTE ST 2084 PQ constants (match qdrv-core/src/pq.rs).
const M1 : f32 = 0.1593017578125;
const M2 : f32 = 78.84375;
const C1 : f32 = 0.8359375;
const C2 : f32 = 18.8515625;
const C3 : f32 = 18.6875;
const PQ_MAX_NITS : f32 = 10000.0;

// PQ signal [0,1] -> normalised linear luminance [0,1].
fn pq_eotf(e : f32) -> f32 {
  let ep = pow(max(e, 0.0), 1.0 / M2);
  let num = max(ep - C1, 0.0);
  let den = C2 - C3 * ep;
  return pow(num / den, 1.0 / M1);
}

// Normalised linear luminance [0,1] -> PQ signal [0,1].
fn pq_oetf(y : f32) -> f32 {
  let ym = pow(max(y, 0.0), M1);
  return pow((C1 + C2 * ym) / (1.0 + C3 * ym), M2);
}

// Linearly interpolated lookup over the evenly spaced tone-curve LUT.
fn lut_sample(x : f32) -> f32 {
  let n = params.lut_size;
  let xc = clamp(x, 0.0, 1.0);
  let pos = xc * f32(n - 1u);
  let i0 = u32(floor(pos));
  let i1 = min(i0 + 1u, n - 1u);
  let t = pos - f32(i0);
  return mix(lut[i0], lut[i1], t);
}

fn map_channel(pq_in : f32) -> f32 {
  let linear_nits = pq_eotf(pq_in) * PQ_MAX_NITS;
  let normalised = clamp(linear_nits / max(params.ref_max, 1.0), 0.0, 1.0);
  let mapped = lut_sample(normalised);
  let output_nits = clamp(mapped * params.target_max, params.target_min, params.target_max);
  return pq_oetf(output_nits / PQ_MAX_NITS);
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid : vec3<u32>) {
  let idx = gid.x;
  if (idx >= params.pixel_count) {
    return;
  }
  let base = idx * 3u;
  output[base + 0u] = map_channel(input[base + 0u]);
  output[base + 1u] = map_channel(input[base + 1u]);
  output[base + 2u] = map_channel(input[base + 2u]);
}
