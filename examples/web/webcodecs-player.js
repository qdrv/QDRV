// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//
// Full in-browser playback pipeline for QDRV (roadmap item 2, browser-runtime
// layer). It loads a delivery `.qdrv32` file directly, or an IVF stream
// produced by `qdrv mux --format ivf`, decodes the AV1 with browser image APIs
// for direct still-picture `.qdrv32` and WebCodecs `VideoDecoder` for IVF,
// recovers one QDRV dynamic-metadata document per frame, converts each decoded
// frame to PQ RGB when the browser can decode it, tone-maps it through the
// shared WebGPU-first renderer, and draws it. If a browser rejects QDRV's AV1
// still-picture profile, the direct `.qdrv32` path renders a clearly labelled
// metadata preview through the same tone mapper instead of failing the example.
//
// EXPERIMENTAL / browser-runtime. IVF playback depends on the browser's
// WebCodecs AV1 support. QDRV delivery streams are AV1 Professional profile,
// 12-bit 4:4:4, BT.2020/PQ; a browser that only decodes 8-bit 4:2:0 AV1 will
// reject the IVF stream at `VideoDecoder.configure()` or `decode()`, surfaced in
// the status line. Direct `.qdrv32` still pictures are not fed to VideoDecoder:
// the harness wraps them as AVIF and uses still-image decode APIs first, then
// falls back to a metadata preview if the browser cannot decode that profile.
// VideoFrame extraction accepts either planar Y/Cb/Cr or packed RGB/BGR output
// because browsers expose different `VideoFrame.format` values.

import init, {
  yuv_ncl_to_pq_rgb,
  extract_stream_metadata,
  parse_qdrv32_container,
  wrap_av1_still_as_avif,
} from "../../qdrv-decode-wasm/pkg/qdrv_decode_wasm.js?v=20260612-qdrv32-preview-fallback";
import { toneMapAdaptive } from "./qdrv-player.js?v=20260612-qdrv32-preview-fallback";

// RFC 6381 codec strings for the QDRV delivery profile (AV1 Professional,
// level 4.0, 12-bit, 4:4:4, BT.2020 primaries / ST 2084 transfer / BT.2020
// NCL matrix, full range). Try only strings that describe QDRV's real 12-bit
// 4:4:4 payloads; do not fall back to a browser-friendly 10-bit/4:2:0 string.
const AV1_CODEC_CANDIDATES = [
  "av01.2.04M.12.0.000.09.16.09.1",
  "av01.2.04M.12.0.000.09.16.09.128",
  "av01.2.04M.12",
];

const statusEl = document.getElementById("ivfStatus");
const fileEl = document.getElementById("ivfFile");
const playEl = document.getElementById("playIvf");
const canvasEl = document.getElementById("ivfOut");
const WEB_HARNESS_BUILD = "20260612-qdrv32-preview-fallback";

function status(msg) {
  if (statusEl) statusEl.textContent = msg;
}

async function selectAv1DecoderConfig(width, height) {
  if (!("VideoDecoder" in window)) {
    throw new Error("this browser does not expose the WebCodecs VideoDecoder API");
  }

  const fallback = {
    codec: AV1_CODEC_CANDIDATES[0],
    codedWidth: width,
    codedHeight: height,
  };
  if (typeof VideoDecoder.isConfigSupported !== "function") {
    return {
      config: fallback,
      attempted: ["VideoDecoder.isConfigSupported unavailable; will try configure() directly"],
    };
  }

  const attempted = [];
  for (const codec of AV1_CODEC_CANDIDATES) {
    const candidate = { codec, codedWidth: width, codedHeight: height };
    try {
      const support = await VideoDecoder.isConfigSupported(candidate);
      attempted.push(`${codec}: ${support.supported ? "supported" : "unsupported"}`);
      if (support.supported) {
        return { config: support.config ?? candidate, attempted };
      }
    } catch (e) {
      attempted.push(`${codec}: ${e.name ?? "Error"}: ${e.message ?? e}`);
    }
  }

  throw new Error(
    "this browser's WebCodecs decoder does not support AV1 Professional " +
      "profile 12-bit 4:4:4, which QDRV delivery streams require; tried " +
      attempted.join("; ") +
      "; the synthetic tone-mapping demo above still works",
  );
}

async function reportInitialWebCodecsSupport() {
  let imageSupport;
  const bitmapSupport =
    typeof createImageBitmap === "function"
      ? "createImageBitmap available"
      : "createImageBitmap unavailable";
  if (!("ImageDecoder" in window)) {
    imageSupport = `direct .qdrv32: ImageDecoder unavailable; ${bitmapSupport}`;
  } else if (typeof ImageDecoder.isTypeSupported === "function") {
    try {
      const supported = await ImageDecoder.isTypeSupported("image/avif");
      imageSupport = `direct .qdrv32: AVIF ImageDecoder ${
        supported ? "supported" : "unsupported"
      }; ${bitmapSupport}`;
    } catch (e) {
      imageSupport = `direct .qdrv32: AVIF ImageDecoder check failed (${
        e.message ?? e
      }); ${bitmapSupport}`;
    }
  } else {
    imageSupport = `direct .qdrv32: ImageDecoder present; AVIF support will be tried at decode; ${bitmapSupport}`;
  }

  let videoSupport;
  try {
    const { config, attempted } = await selectAv1DecoderConfig(64, 64);
    const note = attempted.length > 1 ? ` (${attempted.join("; ")})` : "";
    videoSupport = `IVF: VideoDecoder ${config.codec}${note}`;
  } catch (e) {
    videoSupport = `IVF: ${e.message ?? e}`;
  }
  status(`Harness ${WEB_HARNESS_BUILD}. ${imageSupport}. ${videoSupport}.`);
}

// Parse an IVF container into AV1 temporal units. The parser is strict about
// declared header and frame sizes because this runs on user-selected files.
function parseIvf(buffer) {
  const dv = new DataView(buffer);
  const u8 = new Uint8Array(buffer);
  if (
    buffer.byteLength < 32 ||
    String.fromCharCode(u8[0], u8[1], u8[2], u8[3]) !== "DKIF"
  ) {
    throw new Error("not an IVF file (missing DKIF signature)");
  }

  const headerLen = dv.getUint16(6, true);
  const width = dv.getUint16(12, true);
  const height = dv.getUint16(14, true);
  if (headerLen < 32 || headerLen > buffer.byteLength) {
    throw new Error(`invalid IVF header length ${headerLen}`);
  }
  if (width === 0 || height === 0) {
    throw new Error(`invalid IVF dimensions ${width}x${height}`);
  }

  const frames = [];
  let off = headerLen;
  while (off < buffer.byteLength) {
    if (buffer.byteLength - off < 12) {
      throw new Error("truncated IVF frame header");
    }
    const size = dv.getUint32(off, true);
    const timestamp = Number(dv.getBigUint64(off + 4, true));
    const start = off + 12;
    const end = start + size;
    if (!Number.isSafeInteger(end) || end > buffer.byteLength) {
      throw new Error(`truncated IVF frame payload at offset ${off}`);
    }
    const data = u8.slice(start, end);
    const av1 = inspectAv1Chunk(data, `IVF frame at offset ${off}`);
    frames.push({
      data,
      timestamp,
      type: av1.type,
      stillPicture: av1.stillPicture,
    });
    off = end;
  }
  if (frames.length === 0) {
    throw new Error("IVF file contains no frames");
  }
  ensureFirstChunkIsKey(frames);
  return { width, height, frames };
}

function readAv1Leb128(bytes, offset, limit) {
  let value = 0;
  for (let i = 0; i < 8; i++) {
    if (offset >= limit) {
      throw new Error("truncated AV1 OBU size");
    }
    const byte = bytes[offset++];
    value += (byte & 0x7f) * 2 ** (7 * i);
    if ((byte & 0x80) === 0) {
      if (!Number.isSafeInteger(value)) {
        throw new Error("AV1 OBU size exceeds JavaScript safe integer range");
      }
      return { value, offset };
    }
  }
  throw new Error("AV1 OBU size uses more than 8 LEB128 bytes");
}

function readAv1Bits(bytes, bitOffset, count) {
  let value = 0;
  for (let i = 0; i < count; i++) {
    const byteOffset = bitOffset >> 3;
    if (byteOffset >= bytes.length) {
      throw new Error("truncated AV1 bit field");
    }
    value = (value << 1) | ((bytes[byteOffset] >> (7 - (bitOffset & 7))) & 1);
    bitOffset++;
  }
  return { value, bitOffset };
}

function parseAv1SequenceHeader(payload) {
  let bitOffset = 0;
  let field = readAv1Bits(payload, bitOffset, 3);
  const seqProfile = field.value;
  bitOffset = field.bitOffset;
  field = readAv1Bits(payload, bitOffset, 1);
  const stillPicture = field.value === 1;
  bitOffset = field.bitOffset;
  field = readAv1Bits(payload, bitOffset, 1);
  const reducedStillPictureHeader = field.value === 1;
  return { seqProfile, stillPicture, reducedStillPictureHeader };
}

function classifyAv1FrameHeader(payload, sequenceHeader) {
  if (sequenceHeader?.reducedStillPictureHeader) {
    return "key";
  }

  let bitOffset = 0;
  let field = readAv1Bits(payload, bitOffset, 1);
  const showExistingFrame = field.value === 1;
  bitOffset = field.bitOffset;
  if (showExistingFrame) {
    return "delta";
  }

  field = readAv1Bits(payload, bitOffset, 2);
  const frameType = field.value;
  return frameType === 0 ? "key" : "delta";
}

function inspectAv1Chunk(data, context) {
  let offset = 0;
  let sequenceHeader = null;
  while (offset < data.length) {
    const headerOffset = offset;
    const header = data[offset++];
    if ((header & 0x80) !== 0 || (header & 0x01) !== 0) {
      throw new Error(`${context}: invalid AV1 OBU header at byte ${headerOffset}`);
    }
    const obuType = (header >> 3) & 0x0f;
    const hasExtension = (header & 0x04) !== 0;
    const hasSize = (header & 0x02) !== 0;
    if (hasExtension) {
      if (offset >= data.length) {
        throw new Error(`${context}: truncated AV1 OBU extension header`);
      }
      offset++;
    }

    let payloadEnd;
    if (hasSize) {
      const size = readAv1Leb128(data, offset, data.length);
      offset = size.offset;
      payloadEnd = offset + size.value;
      if (!Number.isSafeInteger(payloadEnd) || payloadEnd > data.length) {
        throw new Error(`${context}: truncated AV1 OBU payload`);
      }
    } else {
      payloadEnd = data.length;
    }

    const payload = data.subarray(offset, payloadEnd);
    if (obuType === 1) {
      sequenceHeader = parseAv1SequenceHeader(payload);
    } else if (obuType === 3 || obuType === 6) {
      return {
        type: classifyAv1FrameHeader(payload, sequenceHeader),
        stillPicture: sequenceHeader?.stillPicture === true,
      };
    }
    offset = payloadEnd;
  }
  throw new Error(`${context}: AV1 chunk contains no frame OBU`);
}

function ensureFirstChunkIsKey(frames) {
  if (frames[0]?.type !== "key") {
    throw new Error(
      "first AV1 chunk is not a key frame; WebCodecs requires a key frame after configure()",
    );
  }
}

function parseQdrv32(buffer) {
  const bytes = new Uint8Array(buffer);
  const manifest = JSON.parse(parse_qdrv32_container(bytes));
  const frames = manifest.frames.map((frame, index) => {
    const start = frame.payload_offset;
    const len = frame.payload_len;
    const end = start + len;
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(len) ||
      !Number.isSafeInteger(end) ||
      start < 0 ||
      len <= 0 ||
      end > bytes.length
    ) {
      throw new Error(`invalid qdrv32 payload range for frame ${index}`);
    }
    const data = bytes.slice(start, end);
    const av1 = inspectAv1Chunk(data, `qdrv32 frame ${index}`);
    return {
      data,
      timestamp: frame.frame_index,
      type: av1.type,
      stillPicture: av1.stillPicture,
    };
  });
  if (frames.length !== manifest.frame_count) {
    throw new Error(
      `qdrv32 manifest frame count mismatch: header=${manifest.frame_count}, parsed=${frames.length}`,
    );
  }
  ensureFirstChunkIsKey(frames);
  return {
    width: manifest.width,
    height: manifest.height,
    frames,
    metadata: manifest.frames.map((frame) => frame.dynamic_metadata),
    label: ".qdrv32",
    decodeMode: "image",
  };
}

function looksLikeQdrv32(buffer, fileName) {
  if (fileName?.toLowerCase().endsWith(".qdrv32")) return true;
  if (buffer.byteLength < 4) return false;
  const u8 = new Uint8Array(buffer, 0, 4);
  return String.fromCharCode(u8[0], u8[1], u8[2], u8[3]) === "QDRV";
}

function parsePlayableInput(buffer, fileName) {
  if (looksLikeQdrv32(buffer, fileName)) {
    return parseQdrv32(buffer);
  }

  const ivf = parseIvf(buffer);
  const total = ivf.frames.reduce((n, f) => n + f.data.length, 0);
  const concat = new Uint8Array(total);
  let at = 0;
  for (const f of ivf.frames) {
    concat.set(f.data, at);
    at += f.data.length;
  }
  const metadata = JSON.parse(extract_stream_metadata(concat));
  if (!Array.isArray(metadata)) {
    throw new Error("metadata extraction did not return an array");
  }
  if (metadata.length !== ivf.frames.length) {
    throw new Error(
      `metadata/frame count mismatch: metadata=${metadata.length}, frames=${ivf.frames.length}`,
    );
  }
  return {
    ...ivf,
    metadata,
    label: "IVF",
    decodeMode: "video",
  };
}

function packedRgbFrameToPqRgb(raw, layout, frame, w, h) {
  const format = frame.format ?? "";
  const channels = {
    RGBA: [0, 1, 2],
    RGBX: [0, 1, 2],
    BGRA: [2, 1, 0],
    BGRX: [2, 1, 0],
  }[format];
  if (!channels || layout.length !== 1) {
    return null;
  }

  const { offset = 0, stride } = layout[0];
  if (stride < w * 4) {
    throw new Error(`packed ${format} stride ${stride} is smaller than width*4`);
  }

  const pqRgb = new Float32Array(w * h * 3);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const src = offset + y * stride + x * 4;
      if (src + 3 >= raw.length) {
        throw new Error(`decoded ${format} plane is truncated`);
      }
      const dst = (y * w + x) * 3;
      pqRgb[dst + 0] = raw[src + channels[0]] / 255;
      pqRgb[dst + 1] = raw[src + channels[1]] / 255;
      pqRgb[dst + 2] = raw[src + channels[2]] / 255;
    }
  }
  return { pqRgb, width: w, height: h, source: `packed ${format}` };
}

// Best-effort decoded VideoFrame extraction. Prefer planar Y'CbCr so the wasm
// Rec. 2100 NCL conversion sees the original QDRV signal, but accept packed
// RGB/BGR frames when a browser has already converted the decoded output.
async function videoFrameToPqRgb(frame) {
  const w = frame.codedWidth;
  const h = frame.codedHeight;
  const size = frame.allocationSize();
  const raw = new Uint8Array(size);
  const layout = await frame.copyTo(raw);
  const packed = packedRgbFrameToPqRgb(raw, layout, frame, w, h);
  if (packed) {
    return packed;
  }
  if (layout.length < 3) {
    throw new Error(
      `expected planar YUV or packed RGB/BGR, got ${layout.length} plane(s) (${frame.format})`,
    );
  }

  const wide = /P1[02]$/.test(frame.format ?? "");
  const maxCode = wide ? 65535 : 255;
  const readPlane = (plane) => {
    const out = new Float32Array(w * h);
    const { offset, stride } = plane;
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        let code;
        if (wide) {
          const p = offset + y * stride + x * 2;
          if (p + 1 >= raw.length) {
            throw new Error("decoded video plane is truncated");
          }
          code = raw[p] | (raw[p + 1] << 8);
        } else {
          const p = offset + y * stride + x;
          if (p >= raw.length) {
            throw new Error("decoded video plane is truncated");
          }
          code = raw[p];
        }
        out[y * w + x] = code / maxCode;
      }
    }
    return out;
  };

  const y = readPlane(layout[0]);
  const cbRaw = readPlane(layout[1]);
  const crRaw = readPlane(layout[2]);
  const cb = cbRaw.map((v) => v - 0.5);
  const cr = crRaw.map((v) => v - 0.5);
  return {
    pqRgb: yuv_ncl_to_pq_rgb(y, cb, cr, w, h),
    width: w,
    height: h,
    source: `planar ${frame.format ?? "YUV"}`,
  };
}

function drawableImageToPqRgb(image, w, h, source) {
  if (!w || !h) {
    throw new Error(`decoded AVIF image has invalid dimensions ${w}x${h}`);
  }

  const scratch = document.createElement("canvas");
  scratch.width = w;
  scratch.height = h;
  const ctx = scratch.getContext("2d", { willReadFrequently: true });
  if (!ctx) {
    throw new Error("2D canvas context unavailable for decoded AVIF bitmap");
  }
  ctx.drawImage(image, 0, 0);
  const rgba = ctx.getImageData(0, 0, w, h).data;
  const pqRgb = new Float32Array(w * h * 3);
  for (let p = 0; p < w * h; p++) {
    pqRgb[p * 3 + 0] = rgba[p * 4 + 0] / 255;
    pqRgb[p * 3 + 1] = rgba[p * 4 + 1] / 255;
    pqRgb[p * 3 + 2] = rgba[p * 4 + 2] / 255;
  }
  return { pqRgb, width: w, height: h, source };
}

function imageBitmapToPqRgb(bitmap) {
  return drawableImageToPqRgb(bitmap, bitmap.width, bitmap.height, "AVIF bitmap RGBA");
}

function htmlImageToPqRgb(image) {
  return drawableImageToPqRgb(
    image,
    image.naturalWidth,
    image.naturalHeight,
    "AVIF HTMLImageElement RGBA",
  );
}

function qdrv32MetadataPreviewToPqRgb(width, height, frameIndex, frameCount) {
  const w = Number.isSafeInteger(width) && width > 0 ? width : 64;
  const h = Number.isSafeInteger(height) && height > 0 ? height : 64;
  const pqRgb = new Float32Array(w * h * 3);
  const framePhase = frameCount > 1 ? frameIndex / (frameCount - 1) : 0;
  for (let y = 0; y < h; y++) {
    const vertical = h > 1 ? y / (h - 1) : 0;
    for (let x = 0; x < w; x++) {
      const horizontal = w > 1 ? x / (w - 1) : 0;
      const signal = Math.min(1, Math.max(0, horizontal * 0.9 + vertical * 0.05 + framePhase * 0.05));
      const dst = (y * w + x) * 3;
      pqRgb[dst + 0] = signal;
      pqRgb[dst + 1] = signal;
      pqRgb[dst + 2] = signal;
    }
  }
  return {
    pqRgb,
    width: w,
    height: h,
    source: "metadata preview PQ ramp",
    preview: true,
  };
}

async function decodeAvifWithImageBitmap(avif) {
  if (typeof createImageBitmap !== "function") {
    throw new Error("createImageBitmap is unavailable");
  }
  const blob = new Blob([avif], { type: "image/avif" });
  let bitmap;
  try {
    bitmap = await createImageBitmap(blob, { colorSpaceConversion: "none" });
  } catch {
    bitmap = await createImageBitmap(blob);
  }
  try {
    return imageBitmapToPqRgb(bitmap);
  } finally {
    bitmap.close?.();
  }
}

async function decodeAvifWithHtmlImage(avif) {
  if (!("Image" in window) || !("URL" in window)) {
    throw new Error("HTMLImageElement Blob decode is unavailable");
  }
  const blob = new Blob([avif], { type: "image/avif" });
  const url = URL.createObjectURL(blob);
  try {
    const image = new Image();
    image.decoding = "async";
    await new Promise((resolve, reject) => {
      image.onload = resolve;
      image.onerror = () => reject(new Error("HTMLImageElement failed to load AVIF"));
      image.src = url;
    });
    return htmlImageToPqRgb(image);
  } finally {
    URL.revokeObjectURL(url);
  }
}

function drawPqAs8Bit(pq, w, h) {
  canvasEl.width = w;
  canvasEl.height = h;
  const ctx = canvasEl.getContext("2d");
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

async function playInput(buffer, fileName, maxNits, minNits) {
  const { width, height, frames, metadata, label, decodeMode } = parsePlayableInput(
    buffer,
    fileName,
  );
  status(`${label}: ${width}x${height}, ${frames.length} frame(s). Decoding...`);

  let index = 0;
  let drawn = 0;
  let processingError = null;
  let decoderError = null;
  const pending = new Set();

  const processFrame = async (frame) => {
    // Stage labels so a failure names exactly where it happened instead of
    // surfacing as a bare browser exception.
    let frameStage = "reading metadata";
    try {
      const i = index++;
      const meta = metadata[i];
      if (!meta) {
        throw new Error(`missing metadata for decoded frame ${i}`);
      }
      frameStage = `extracting decoded pixels (format=${frame.format})`;
      const { pqRgb, width: fw, height: fh, source } = await videoFrameToPqRgb(frame);
      frameStage = "tone mapping";
      const { pixels: mapped, path } = await toneMapAdaptive(
        pqRgb,
        fw,
        fh,
        JSON.stringify(meta),
        maxNits,
        minNits,
      );
      frameStage = "drawing";
      drawPqAs8Bit(mapped, fw, fh);
      drawn++;
      status(`Decoded ${source} + tone-mapped frame ${drawn}/${frames.length} via ${path}.`);
    } catch (e) {
      throw new Error(
        `frame ${index - 1}, while ${frameStage}: ${e.name ?? "Error"}: ${e.message ?? e}`,
      );
    } finally {
      frame.close();
    }
  };

  const processDecodedPixels = async ({ pqRgb, width: fw, height: fh, source, preview = false }) => {
    let frameStage = "reading metadata";
    try {
      const i = index++;
      const meta = metadata[i];
      if (!meta) {
        throw new Error(`missing metadata for decoded frame ${i}`);
      }
      frameStage = "tone mapping";
      const { pixels: mapped, path } = await toneMapAdaptive(
        pqRgb,
        fw,
        fh,
        JSON.stringify(meta),
        maxNits,
        minNits,
      );
      frameStage = "drawing";
      drawPqAs8Bit(mapped, fw, fh);
      drawn++;
      const verb = preview ? "Rendered" : "Decoded";
      status(`${verb} ${source} + tone-mapped frame ${drawn}/${frames.length} via ${path}.`);
    } catch (e) {
      throw new Error(
        `frame ${index - 1}, while ${frameStage}: ${e.name ?? "Error"}: ${e.message ?? e}`,
      );
    }
  };

  const track = (promise) => {
    pending.add(promise);
    promise
      .catch((e) => {
        processingError = e;
        status(`Frame processing error: ${e.message ?? e}`);
      })
      .finally(() => pending.delete(promise));
  };

  if (decodeMode === "image") {
    for (let i = 0; i < frames.length; i++) {
      status(`Wrapping .qdrv32 frame ${i + 1}/${frames.length} as AVIF for still-image decode...`);
      const avif = wrap_av1_still_as_avif(frames[i].data, width, height);
      const failures = [];
      const retryIndex = index;
      const retryDrawn = drawn;

      if ("ImageDecoder" in window) {
        try {
          const decoder = new ImageDecoder({
            data: avif,
            type: "image/avif",
            colorSpaceConversion: "none",
          });
          try {
            const { image } = await decoder.decode({ frameIndex: 0 });
            await processFrame(image);
            continue;
          } finally {
            decoder.close();
          }
        } catch (e) {
          index = retryIndex;
          drawn = retryDrawn;
          processingError = null;
          failures.push(`ImageDecoder: ${e.name ?? "Error"}: ${e.message ?? e}`);
        }
      } else {
        failures.push("ImageDecoder: unavailable");
      }

      try {
        const decoded = await decodeAvifWithImageBitmap(avif);
        await processDecodedPixels(decoded);
        continue;
      } catch (e) {
        index = retryIndex;
        drawn = retryDrawn;
        processingError = null;
        failures.push(`createImageBitmap: ${e.name ?? "Error"}: ${e.message ?? e}`);
      }

      try {
        const decoded = await decodeAvifWithHtmlImage(avif);
        await processDecodedPixels(decoded);
      } catch (e) {
        index = retryIndex;
        drawn = retryDrawn;
        processingError = null;
        failures.push(`HTMLImageElement: ${e.name ?? "Error"}: ${e.message ?? e}`);
        console.warn(
          `Direct .qdrv32 frame ${i} could not be decoded as AVIF; rendering metadata preview instead.`,
          failures,
        );
        status(
          `.qdrv32 frame ${i + 1}/${frames.length}: browser image decoders rejected the AV1 still image; rendering metadata preview...`,
        );
        const preview = qdrv32MetadataPreviewToPqRgb(width, height, i, frames.length);
        await processDecodedPixels(preview);
      }
    }
    if (drawn !== frames.length) {
      throw new Error(`decoded ${drawn} frame(s), expected ${frames.length}`);
    }
    return;
  }

  const decoder = new VideoDecoder({
    output: (frame) => {
      track(processFrame(frame));
    },
    error: (e) => {
      decoderError = e;
      status(`Decoder error callback (${e.name ?? "error"}): ${e.message ?? e}`);
    },
  });

  // Ask the browser up front instead of discovering mid-decode. An
  // unsupported configuration closes the decoder via its async error
  // callback, and every later decode() call on the closed decoder then
  // throws Firefox's cryptic "object is no longer usable" InvalidStateError,
  // masking the real reason.
  status("Checking WebCodecs support for AV1 Professional 12-bit 4:4:4...");
  const { config } = await selectAv1DecoderConfig(width, height);
  status(`WebCodecs reports ${config.codec} supported; decoding...`);

  let stage = "configuring decoder";
  try {
    decoder.configure(config);
    stage = "decoding";
    for (let i = 0; i < frames.length; i++) {
      // Stop feeding a decoder that has already failed; its own error
      // message is the one worth surfacing, not a closed-decoder throw.
      if (decoderError || decoder.state === "closed") {
        break;
      }
      decoder.decode(
        new EncodedVideoChunk({
          type: frames[i].type ?? (i === 0 ? "key" : "delta"),
          timestamp: frames[i].timestamp,
          data: frames[i].data,
        }),
      );
    }
    if (!decoderError && decoder.state !== "closed") {
      stage = "flushing decoder";
      await decoder.flush();
    }
    stage = "finishing frame processing";
    await Promise.allSettled([...pending]);
  } catch (e) {
    // Prefer the decoder's own error when both exist; a throw from a closed
    // decoder is a symptom, not the cause.
    const cause = decoderError ?? e;
    throw new Error(
      `while ${stage}: ${cause.name ?? "Error"}: ${cause.message ?? cause}`,
    );
  } finally {
    if (decoder.state !== "closed") {
      decoder.close();
    }
  }

  if (decoderError) {
    throw new Error(
      `decoder failed (${decoderError.name ?? "error"}): ${decoderError.message ?? decoderError}`,
    );
  }
  if (processingError) {
    throw processingError;
  }
  if (drawn !== frames.length) {
    throw new Error(`decoded ${drawn} frame(s), expected ${frames.length}`);
  }
}

async function main() {
  await init({
    module_or_path: new URL(
      `../../qdrv-decode-wasm/pkg/qdrv_decode_wasm_bg.wasm?v=${WEB_HARNESS_BUILD}`,
      import.meta.url,
    ),
  });
  if (!playEl) return;
  playEl.disabled = false;
  await reportInitialWebCodecsSupport();

  playEl.addEventListener("click", async () => {
    const file = fileEl?.files?.[0];
    if (!file) {
      status("Choose a .qdrv32 file, or an .ivf file produced with `qdrv mux --format ivf`.");
      return;
    }
    try {
      const maxNits = parseFloat(document.getElementById("maxNits")?.value ?? "600");
      const minNits = parseFloat(document.getElementById("minNits")?.value ?? "0.1");
      const buffer = await file.arrayBuffer();
      await playInput(buffer, file.name, maxNits, minNits);
    } catch (e) {
      status(`Error: ${e.message ?? e}`);
    }
  });
}

main().catch((e) => status(`Failed to initialise: ${e.message ?? e}`));
