// Fast wasm correctness gate (no timing) — CI-friendly. Asserts three things
// across fixtures × levels:
//   1. simd128 payload output is BYTE-IDENTICAL to the scalar payload (the
//      project's scalar-vs-SIMD bit-identity rule for SIMD kernels).
//   2. round-trip: each payload decodes its own frame back to the input.
//   3. interop with the C reference (@bokuweb/zstd-wasm): our frames decode in
//      bokuweb, and bokuweb's frames decode in ours (#348 acceptance).
// Exits non-zero on the first mismatch.
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

async function loadOurPayload(dir) {
  const glue = await import(`../npm/${dir}/structured_zstd_wasm.js`);
  const bytes = await readFile(here(`../npm/${dir}/structured_zstd_wasm_bg.wasm`));
  await glue.default({ module_or_path: bytes });
  return { compress: glue.compress, decompress: glue.decompress };
}
async function loadBokuweb() {
  const m = await import("@bokuweb/zstd-wasm");
  await m.init();
  return { compress: m.compress, decompress: m.decompress };
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
const fail = (msg) => { console.error(`FAIL: ${msg}`); failures++; };

for (const [name, data] of fixtures()) {
  for (const level of [-3, 1, 3, 9, 19, 22]) {
    const cs = simd.compress(data, level);
    const cc = scalar.compress(data, level);
    // 1. byte-identity simd128 vs scalar
    if (!eq(cs, cc)) fail(`${name} L${level}: simd128 output != scalar output (${cs.length} vs ${cc.length})`);
    // 2. round-trip on both payloads
    if (!eq(simd.decompress(cs), data)) fail(`${name} L${level}: simd round-trip`);
    if (!eq(scalar.decompress(cc), data)) fail(`${name} L${level}: scalar round-trip`);
    // 3. interop with the C reference (skip empty — bokuweb rejects 0-length)
    if (data.length > 0) {
      if (!eq(boku.decompress(cs), data)) fail(`${name} L${level}: bokuweb cannot decode our frame`);
      const cb = boku.compress(data, level);
      if (!eq(simd.decompress(cb), data)) fail(`${name} L${level}: we cannot decode bokuweb's frame`);
    }
  }
}

if (failures > 0) {
  console.error(`\n${failures} failure(s)`);
  process.exit(1);
}
console.log("wasm correctness OK — simd128==scalar byte-identical, round-trip + C interop verified");
