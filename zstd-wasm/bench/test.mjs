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
    CompressStreamCtor: glue.ZstdCompressStream,
    Dictionary: glue.ZstdDictionary,
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
      // Both directions, both payloads — a scalar-only host must interop too.
      if (!eq(boku.decompress(cs), data)) fail(`${name} L${level}: C ref cannot decode our simd frame`);
      if (!eq(boku.decompress(cc), data)) fail(`${name} L${level}: C ref cannot decode our scalar frame`);
      const cb = boku.compress(data, level);
      if (!eq(simd.decompress(cb), data)) fail(`${name} L${level}: simd cannot decode C ref's frame`);
      if (!eq(scalar.decompress(cb), data)) fail(`${name} L${level}: scalar cannot decode C ref's frame`);
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

// --- Prepared dictionary: hot-path reuse must match the one-shot dict API ---
// One ZstdDictionary serves repeated one-shot calls (cached primed encoder),
// streams on both sides (no per-stream re-parse), and the C reference decodes
// every frame it produces.
for (const payload of [simd, scalar]) {
  const prepared = new payload.Dictionary(dict);
  if (prepared.id === 0) fail("prepared dict: trained dictionary must carry an ID");
  for (const sample of ["sample-1.service", "sample-2.service"]) {
    const data = new Uint8Array(await readFile(here(`fixtures/${sample}`)));
    for (const level of [3, 19]) {
      // Repeated calls exercise the cached-compressor path (2nd call reuses
      // the primed snapshot) and must agree with the parse-per-call API.
      const first = prepared.compress(data, level);
      const second = prepared.compress(data, level);
      if (!eq(prepared.decompress(first), data)) fail(`prepared dict L${level}: round-trip (1st)`);
      if (!eq(prepared.decompress(second), data)) fail(`prepared dict L${level}: round-trip (2nd)`);
      const dctx = boku.createDCtx();
      if (!eq(boku.decompressUsingDict(dctx, second, dict), data))
        fail(`prepared dict L${level}: C ref cannot decode the cached-path frame`);
      boku.freeDCtx(dctx);
      if (!eq(prepared.decompress(payload.compressUsingDict(data, dict, level)), data))
        fail(`prepared dict L${level}: cannot decode the byte-API dict frame`);
    }
    // Streams seeded from the prepared dictionary round-trip through the
    // prepared decode stream.
    const cs = payload.CompressStreamCtor.withPreparedDictionary(3, prepared);
    const out = [cs.push(data), cs.finish()];
    cs.free();
    const frame = Buffer.concat(out.map((p) => Buffer.from(p)));
    const ds = payload.StreamCtor.withPreparedDictionary(prepared);
    const back = Buffer.concat([Buffer.from(ds.push(new Uint8Array(frame))), Buffer.from(ds.finish())]);
    ds.free();
    if (!eq(new Uint8Array(back), data)) fail(`prepared dict stream: ${sample} round-trip`);
  }
  // Raw-content dictionaries: id 0, frames carry no dictionary ID, and the
  // C reference decodes them with the same raw bytes.
  const rawDict = new Uint8Array(await readFile(here("fixtures/sample-1.service")));
  const rawPrepared = new payload.Dictionary(rawDict);
  if (rawPrepared.id !== 0) fail("prepared raw dict: id must be 0");
  const tail = new TextEncoder().encode("unique raw-dict tail 0123456789");
  const rawData = new Uint8Array(rawDict.length + tail.length);
  rawData.set(rawDict); rawData.set(tail, rawDict.length);
  const rawFrame = rawPrepared.compress(rawData, 3);
  if (!eq(rawPrepared.decompress(rawFrame), rawData)) fail("prepared raw dict: round-trip");
  const dctx = boku.createDCtx();
  if (!eq(boku.decompressUsingDict(dctx, rawFrame, rawDict), rawData))
    fail("prepared raw dict: C ref cannot decode the raw-content frame");
  boku.freeDCtx(dctx);
  // Raw-content STREAMS: the frames carry no dictionary ID, so this is the
  // path where the prepared decode stream must force the dictionary per
  // frame; the C reference cross-checks the produced stream bytes.
  {
    const cs = payload.CompressStreamCtor.withPreparedDictionary(3, rawPrepared);
    const streamFrame = Buffer.concat([
      Buffer.from(cs.push(rawData)),
      Buffer.from(cs.finish()),
    ]);
    cs.free();
    const ds = payload.StreamCtor.withPreparedDictionary(rawPrepared);
    const back = Buffer.concat([
      Buffer.from(ds.push(new Uint8Array(streamFrame))),
      Buffer.from(ds.finish()),
    ]);
    ds.free();
    if (!eq(new Uint8Array(back), rawData)) fail("prepared raw dict stream: round-trip");
    const dctx2 = boku.createDCtx();
    if (!eq(boku.decompressUsingDict(dctx2, new Uint8Array(streamFrame), rawDict), rawData))
      fail("prepared raw dict stream: C ref cannot decode the ID-less stream frame");
    boku.freeDCtx(dctx2);
  }
  rawPrepared.free();
  prepared.free();
}

// --- npm wrapper surface (built index.js): the public API users import ------
// The blocks above exercise the bindgen glue directly; regressions in the
// TS wrapper (ZstdDict class, stream factories) would slip through, so run
// one round-trip through the built wrapper as well.
{
  const npm = await import("../npm/index.js");
  const dict = new Uint8Array(await readFile(here("fixtures/service.dict")));
  const data = new Uint8Array(await readFile(here("fixtures/sample-1.service")));
  const zd = await npm.ZstdDict.create(dict);
  if (zd.id === 0) fail("npm wrapper: trained dictionary must carry an ID");
  const frame = zd.compress(data, 3);
  if (!eq(zd.decompress(frame), data)) fail("npm wrapper: one-shot round-trip");
  const cs = zd.compressStream(3);
  const streamed = Buffer.concat([Buffer.from(cs.push(data)), Buffer.from(cs.finish())]);
  cs.free();
  const ds = zd.decompressStream();
  const back = Buffer.concat([
    Buffer.from(ds.push(new Uint8Array(streamed))),
    Buffer.from(ds.finish()),
  ]);
  ds.free();
  if (!eq(new Uint8Array(back), data)) fail("npm wrapper: stream round-trip");
  zd.free();

  // Raw-content dictionary through the same public API: id 0 and ID-less
  // frames, so the wrapper's prepared decode stream must force the
  // dictionary per frame.
  const rawZd = await npm.ZstdDict.create(data);
  if (rawZd.id !== 0) fail("npm wrapper: raw-content dict id must be 0");
  const rawTail = new TextEncoder().encode("npm raw tail 0123456789");
  const rawData = new Uint8Array(data.length + rawTail.length);
  rawData.set(data); rawData.set(rawTail, data.length);
  const rawFrame = rawZd.compress(rawData, 3);
  if (!eq(rawZd.decompress(rawFrame), rawData)) fail("npm wrapper: raw one-shot round-trip");
  const rcs = rawZd.compressStream(3);
  const rawStreamed = Buffer.concat([Buffer.from(rcs.push(rawData)), Buffer.from(rcs.finish())]);
  rcs.free();
  const rds = rawZd.decompressStream();
  const rawBack = Buffer.concat([
    Buffer.from(rds.push(new Uint8Array(rawStreamed))),
    Buffer.from(rds.finish()),
  ]);
  rds.free();
  if (!eq(new Uint8Array(rawBack), rawData)) fail("npm wrapper: raw stream round-trip");
  rawZd.free();
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

// --- Streaming compressor: chunked plaintext must produce a valid frame -----
// Feed each payload to ZstdCompressStream in several chunk granularities and
// concatenate the emitted compressed bytes; the resulting frame omits FCS
// (unknown size while streaming). MANDATORY: it must decode back to the input
// BOTH in our own decoder AND in the C reference (@bokuweb/zstd-wasm) — output
// need NOT be byte-identical to one-shot, only decode to the same bytes.
function streamCompress(ctor, data, level, chunkSize) {
  const s = new ctor(level);
  const parts = [];
  for (let i = 0; i < data.length; i += chunkSize) {
    parts.push(s.push(data.subarray(i, Math.min(i + chunkSize, data.length))));
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
  if (data.length === 0) continue; // empty input: covered by one-shot; skip sweep
  // L22 advertises the 128 MiB max window verbatim in the no-FCS streaming
  // header — included here to pin that our own decoder (and the C reference)
  // accept it, i.e. the encoder↔decoder window cap stays aligned.
  for (const level of [-3, 1, 3, 19, 22]) {
    for (const chunk of [1, 3, 7, 64, data.length]) {
      const simdFrame = streamCompress(simd.CompressStreamCtor, data, level, chunk);
      const scalarFrame = streamCompress(scalar.CompressStreamCtor, data, level, chunk);
      // MANDATORY: our decoder round-trips the streamed frame.
      if (!eq(simd.decompress(simdFrame), data)) {
        fail(`compress-stream ${name} L${level} chunk=${chunk}: simd round-trip`);
      }
      if (!eq(scalar.decompress(scalarFrame), data)) {
        fail(`compress-stream ${name} L${level} chunk=${chunk}: scalar round-trip`);
      }
      // MANDATORY: the C reference decodes our no-FCS streamed frame (both payloads).
      if (!eq(boku.decompress(simdFrame), data)) {
        fail(`compress-stream ${name} L${level} chunk=${chunk}: C ref cannot decode our simd frame`);
      }
      if (!eq(boku.decompress(scalarFrame), data)) {
        fail(`compress-stream ${name} L${level} chunk=${chunk}: C ref cannot decode our scalar frame`);
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
