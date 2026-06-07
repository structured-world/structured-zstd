// Fast wasm correctness gate (no timing) — CI-friendly.
//
// MANDATORY (failure → non-zero exit):
//   1. round-trip: each payload decodes its own frame back to the input.
//   2. FORMAT CROSS-CHECK with the C reference (@bokuweb/zstd-wasm): our
//      frames decode in bokuweb, and bokuweb's frames decode in ours. This is
//      the contract that matters — valid, interoperable zstd frames. A broken
//      simd128 kernel (wrong match mask / bad copy) corrupts the frame and is
//      caught here.
//
// INFORMATIONAL (logged, never fails): whether the simd128 payload's bytes
// match the scalar payload's. We do NOT require byte-identical output (the
// drop-in contract is wire-format validity, not byte parity) — but a
// divergence between our own two payloads is worth surfacing.
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

async function loadOurPayload(dir) {
  const glue = await import(`../npm/${dir}/structured_zstd_wasm.js`);
  const bytes = await readFile(here(`../npm/${dir}/structured_zstd_wasm_bg.wasm`));
  await glue.default({ module_or_path: bytes });
  return {
    compress: glue.compress,
    decompress: glue.decompress,
    compressUsingDict: glue.compressUsingDict,
    decompressUsingDict: glue.decompressUsingDict,
    StreamCtor: glue.ZstdDecompressStream,
  };
}
async function loadBokuweb() {
  const m = await import("@bokuweb/zstd-wasm");
  await m.init();
  return {
    compress: m.compress,
    decompress: m.decompress,
    createCCtx: m.createCCtx,
    freeCCtx: m.freeCCtx,
    compressUsingDict: m.compressUsingDict,
    createDCtx: m.createDCtx,
    freeDCtx: m.freeDCtx,
    decompressUsingDict: m.decompressUsingDict,
  };
}

const eq = (a, b) => a.length === b.length && Buffer.from(a).equals(Buffer.from(b));

function fixtures() {
  const enc = new TextEncoder();
  const logs = enc.encode("ts=2026 level=INFO msg=flush tenant=demo table=orders\n".repeat(400));
  const rnd = new Uint8Array(8192);
  let s = 0x1234567 >>> 0;
  for (let i = 0; i < rnd.length; i++) { s ^= s << 13; s >>>= 0; s ^= s >> 17; s ^= s << 5; s >>>= 0; rnd[i] = s & 0xff; }
  const pattern = enc.encode("coordinode:segment:0001|tenant=demo|".repeat(2000));
  return [
    ["log-lines", logs],
    ["random-8k", rnd],
    ["pattern-72k", pattern],
    ["empty", new Uint8Array(0)],
    ["tiny", enc.encode("x")],
  ];
}

const simd = await loadOurPayload("simd");
const scalar = await loadOurPayload("scalar");
const boku = await loadBokuweb();

let failures = 0;
let divergences = 0;
const fail = (msg) => { console.error(`FAIL: ${msg}`); failures++; };

for (const [name, data] of fixtures()) {
  for (const level of [-3, 1, 3, 9, 19, 22]) {
    const cs = simd.compress(data, level);
    const cc = scalar.compress(data, level);
    // MANDATORY: round-trip on both payloads.
    if (!eq(simd.decompress(cs), data)) fail(`${name} L${level}: simd round-trip`);
    if (!eq(scalar.decompress(cc), data)) fail(`${name} L${level}: scalar round-trip`);
    // MANDATORY: format cross-check with the C reference (skip empty —
    // bokuweb rejects 0-length input).
    if (data.length > 0) {
      if (!eq(boku.decompress(cs), data)) fail(`${name} L${level}: C reference cannot decode our frame`);
      const cb = boku.compress(data, level);
      if (!eq(simd.decompress(cb), data)) fail(`${name} L${level}: we cannot decode the C reference's frame`);
    }
    // INFORMATIONAL: note (do not fail) if our two payloads diverge.
    if (!eq(cs, cc)) {
      divergences++;
      console.log(`note: simd128 != scalar bytes for ${name} L${level} (${cs.length} vs ${cc.length}) — allowed`);
    }
  }
}

// --- Dictionary API: round-trip + format cross-check with the C reference ---
// bokuweb's dict API is low-level (createCCtx/freeCCtx); ours is one-shot.
const dict = new Uint8Array(await readFile(here("fixtures/service.dict")));
for (const sample of ["sample-1.service", "sample-2.service"]) {
  const data = new Uint8Array(await readFile(here(`fixtures/${sample}`)));
  for (const level of [3, 19]) {
    const cs = simd.compressUsingDict(data, dict, level);
    const cc = scalar.compressUsingDict(data, dict, level);
    if (!eq(simd.decompressUsingDict(cs, dict), data)) fail(`dict ${sample} L${level}: simd round-trip`);
    if (!eq(scalar.decompressUsingDict(cc, dict), data)) fail(`dict ${sample} L${level}: scalar round-trip`);
    // FORMAT CROSS-CHECK: the C reference decodes our dict frame, and we
    // decode its dict frame.
    const dctx = boku.createDCtx();
    if (!eq(boku.decompressUsingDict(dctx, cs, dict), data)) fail(`dict ${sample} L${level}: C ref cannot decode our dict frame`);
    boku.freeDCtx(dctx);
    const cctx = boku.createCCtx();
    const cb = boku.compressUsingDict(cctx, data, dict, level);
    boku.freeCCtx(cctx);
    if (!eq(simd.decompressUsingDict(cb, dict), data)) fail(`dict ${sample} L${level}: we cannot decode C ref's dict frame`);
    if (!eq(cs, cc)) { divergences++; console.log(`note: dict simd128 != scalar bytes for ${sample} L${level} — allowed`); }
  }
}

// --- Streaming decompressor: chunked input must equal one-shot output -------
// Feed each frame to ZstdDecompressStream in several chunk granularities
// (incl. 1-byte, which exercises the mid-block buffering / block-boundary
// gate hardest) and assert the concatenated output matches one-shot decode.
function streamDecode(ctor, framed, chunkSize) {
  const s = new ctor();
  const parts = [];
  for (let i = 0; i < framed.length; i += chunkSize) {
    parts.push(s.push(framed.subarray(i, Math.min(i + chunkSize, framed.length))));
  }
  parts.push(s.finish());
  s.free();
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) { out.set(p, off); off += p.length; }
  return out;
}
for (const [name, data] of fixtures()) {
  if (data.length === 0) continue; // empty frame: trivial, skip chunk sweep
  for (const level of [1, 3, 19]) {
    const framed = simd.compress(data, level);
    for (const chunk of [1, 3, 7, 64, framed.length]) {
      if (!eq(streamDecode(simd.StreamCtor, framed, chunk), data)) {
        fail(`stream ${name} L${level} chunk=${chunk}: simd streaming != one-shot`);
      }
      if (!eq(streamDecode(scalar.StreamCtor, framed, chunk), data)) {
        fail(`stream ${name} L${level} chunk=${chunk}: scalar streaming != one-shot`);
      }
    }
  }
}

if (failures > 0) {
  console.error(`\n${failures} format/round-trip failure(s)`);
  process.exit(1);
}
console.log(
  `wasm format cross-check OK — round-trip + C-reference interop verified` +
    (divergences > 0 ? ` (${divergences} simd128/scalar byte divergences, allowed)` : `; simd128 bytes matched scalar`),
);
