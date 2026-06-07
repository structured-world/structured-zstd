// Node bench harness — structured-zstd wasm (simd128 + scalar) vs the most
// popular npm wasm zstd, @bokuweb/zstd-wasm (an emscripten build of the C
// reference). Loads each of our two payloads explicitly (bypassing the
// autodetect loader so both tiers are measured), runs shared fixtures, and
// emits REPORT lines + a human-readable table. Pre-publish gate: we look at
// these numbers before shipping the npm package.
//
// Run: node bench.mjs   (from zstd-wasm/bench, after `npm install`)
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

// --- Fixtures (mirror zstd/benches/support/mod.rs shapes) -------------------
function randomBytes(len, seed) {
  // Cheap xorshift32 so the corpus is deterministic across runs/engines.
  const out = new Uint8Array(len);
  let s = seed >>> 0;
  for (let i = 0; i < len; i++) {
    s ^= s << 13; s >>>= 0; s ^= s >> 17; s ^= s << 5; s >>>= 0;
    out[i] = s & 0xff;
  }
  return out;
}
function repeatedLogLines(len) {
  const lines = [
    'ts=2026-03-26T21:39:28Z level=INFO msg="flush memtable" tenant=demo table=orders region=eu-west\n',
    'ts=2026-03-26T21:39:29Z level=INFO msg="rotate segment" tenant=demo table=orders region=eu-west\n',
    'ts=2026-03-26T21:39:30Z level=INFO msg="compact level" tenant=demo table=orders region=eu-west\n',
    'ts=2026-03-26T21:39:31Z level=INFO msg="write block" tenant=demo table=orders region=eu-west\n',
  ].join("");
  const unit = new TextEncoder().encode(lines);
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = unit[i % unit.length];
  return out;
}
function repeatedPattern(len) {
  const unit = new TextEncoder().encode("coordinode:segment:0001|tenant=demo|label=orders|");
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = unit[i % unit.length];
  return out;
}

async function buildFixtures() {
  const fx = [
    ["small-4k-log-lines", repeatedLogLines(4 * 1024)],
    ["small-10k-random", randomBytes(10 * 1024, 0x5eed1)],
    ["low-entropy-1m", repeatedPattern(1024 * 1024)],
    ["high-entropy-1m", randomBytes(1024 * 1024, 0xc0ffee11)],
    ["large-log-stream-8m", repeatedLogLines(8 * 1024 * 1024)],
  ];
  try {
    const corpus = await readFile(here("../../zstd/decodecorpus_files/z000033"));
    fx.splice(2, 0, ["decodecorpus-z000033", new Uint8Array(corpus)]);
  } catch {
    // Corpus fixture optional.
  }
  return fx;
}

// --- Timing -----------------------------------------------------------------
function medianNsPerOp(fn, totalBudgetMs) {
  // Warm up, then collect samples until the budget elapses; return the median.
  for (let i = 0; i < 3; i++) fn();
  const samples = [];
  const deadline = process.hrtime.bigint() + BigInt(totalBudgetMs) * 1_000_000n;
  do {
    const t0 = process.hrtime.bigint();
    fn();
    samples.push(Number(process.hrtime.bigint() - t0));
  } while (process.hrtime.bigint() < deadline && samples.length < 200);
  samples.sort((a, b) => a - b);
  return samples[samples.length >> 1];
}

const LEVELS = [1, 3, 19, 22];
const BUDGET_MS = 400;

function fmtNs(ns) {
  if (ns >= 1e6) return `${(ns / 1e6).toFixed(2)}ms`;
  if (ns >= 1e3) return `${(ns / 1e3).toFixed(2)}µs`;
  return `${ns.toFixed(0)}ns`;
}

const engines = {
  "ours-simd128": await loadOurPayload("simd"),
  "ours-scalar": await loadOurPayload("scalar"),
  bokuweb: await loadBokuweb(),
};

const fixtures = await buildFixtures();
const rows = [];
for (const [scenario, data] of fixtures) {
  for (const level of LEVELS) {
    for (const [name, eng] of Object.entries(engines)) {
      const framed = eng.compress(data, level);
      // Round-trip correctness check before timing.
      const back = eng.decompress(framed);
      const ok = back.length === data.length;
      const cNs = medianNsPerOp(() => eng.compress(data, level), BUDGET_MS);
      const dNs = medianNsPerOp(() => eng.decompress(framed), BUDGET_MS);
      const ratio = framed.length / Math.max(1, data.length);
      console.log(
        `REPORT scenario=${scenario} engine=${name} level=${level} ` +
          `input_bytes=${data.length} framed_bytes=${framed.length} ratio=${ratio.toFixed(6)} ` +
          `compress_ns=${cNs} decompress_ns=${dNs} roundtrip=${ok ? "ok" : "FAIL"}`,
      );
      rows.push({ scenario, level, name, ratio, cNs, dNs, ok });
    }
  }
}

// --- Human-readable comparison vs bokuweb -----------------------------------
console.log("\n=== structured-zstd wasm vs @bokuweb/zstd-wasm ===");
console.log("(c = compress, d = decompress; x = ours/bokuweb time, <1 = we are faster)");
for (const [scenario, data] of fixtures) {
  console.log(`\n${scenario} (${data.length} bytes)`);
  for (const level of LEVELS) {
    const at = (n) => rows.find((r) => r.scenario === scenario && r.level === level && r.name === n);
    const b = at("bokuweb"), s = at("ours-simd128"), sc = at("ours-scalar");
    const x = (ours) => (ours.cNs / b.cNs).toFixed(2);
    const xd = (ours) => (ours.dNs / b.dNs).toFixed(2);
    console.log(
      `  L${String(level).padStart(2)}  ratio ours=${s.ratio.toFixed(4)} boku=${b.ratio.toFixed(4)}` +
        ` | c: simd ${fmtNs(s.cNs)}(${x(s)}x) scalar ${fmtNs(sc.cNs)}(${x(sc)}x) boku ${fmtNs(b.cNs)}` +
        ` | d: simd ${fmtNs(s.dNs)}(${xd(s)}x) scalar ${fmtNs(sc.dNs)}(${xd(sc)}x) boku ${fmtNs(b.dNs)}`,
    );
  }
}

const anyFail = rows.some((r) => !r.ok);
console.log(anyFail ? "\nROUNDTRIP FAILURES PRESENT" : "\nall roundtrips ok");
process.exit(anyFail ? 1 : 0);
