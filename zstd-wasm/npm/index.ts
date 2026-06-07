/**
 * `@structured-world/structured-zstd` — pure-Rust Zstandard codec compiled to
 * WebAssembly, with automatic selection of the SIMD (`simd128`) or scalar
 * payload based on the host engine's capabilities.
 *
 * Pure ESM, strict TypeScript. One async API for browser, Node.js and Deno:
 *
 * ```ts
 * import { compress, decompress } from "@structured-world/structured-zstd";
 * const framed = await compress(new TextEncoder().encode("hello"), 19);
 * const plain = await decompress(framed);
 * ```
 *
 * The codec is initialised lazily on first use; call {@link init} explicitly
 * to pre-warm it (e.g. before a latency-sensitive path).
 */

import { simd } from "wasm-feature-detect";

/** Shape of a wasm-pack `web`-target payload glue module (simd or scalar). */
interface Payload {
  /** Async wasm initialiser. Accepts wasm bytes / a URL, or nothing (browser). */
  // Typed `unknown`: wasm-bindgen's `web` init accepts the broad `InitInput`
  // (bytes / URL / Response / Module), and Node's `fs.readFile` returns
  // `Buffer<ArrayBufferLike>`, wider than the DOM `BufferSource` in the .d.ts.
  default: (moduleOrPath?: unknown) => Promise<unknown>;
  compress: (data: Uint8Array, level: number) => Uint8Array;
  decompress: (data: Uint8Array) => Uint8Array;
}

/** Default compression level when the caller does not pass one. */
const DEFAULT_LEVEL = 3;

let loaded: Payload | undefined;
let loading: Promise<Payload> | undefined;

/** True when running under Node.js (no `fetch` of `file://`, so load bytes). */
function isNode(): boolean {
  const g = globalThis as { process?: { versions?: { node?: string } } };
  return g.process?.versions?.node != null;
}

/** Read a payload's `.wasm` from disk (Node only), relative to this module. */
async function readWasmBytes(dir: "simd" | "scalar"): Promise<Uint8Array> {
  const url = new URL(`./${dir}/structured_zstd_wasm_bg.wasm`, import.meta.url);
  const fs = await import(/* @vite-ignore */ "node:fs/promises");
  return fs.readFile(url);
}

async function load(): Promise<Payload> {
  if (loaded !== undefined) {
    return loaded;
  }
  const useSimd = await simd();
  const dir = useSimd ? "simd" : "scalar";
  const mod = (useSimd
    ? await import("./simd/structured_zstd_wasm.js")
    : await import("./scalar/structured_zstd_wasm.js")) as unknown as Payload;
  if (isNode()) {
    // Node's global fetch rejects `file://`, so hand the init the raw bytes.
    // wasm-bindgen's current init wants `{ module_or_path }` (passing the
    // bytes positionally is deprecated and warns).
    await mod.default({ module_or_path: await readWasmBytes(dir) });
  } else {
    // Browser / Deno: the glue resolves its `.wasm` from its own module URL.
    await mod.default();
  }
  loaded = mod;
  return mod;
}

/**
 * Initialise the codec (select + instantiate the best payload) ahead of time.
 * Idempotent and concurrency-safe; resolves once the module is ready.
 */
export async function init(): Promise<void> {
  loading ??= load();
  await loading;
}

/**
 * Compress `data` into a Zstandard frame at `level` (zstd scale: `1..=22`,
 * negatives for the ultra-fast tier; defaults to {@link DEFAULT_LEVEL}). The
 * frame decodes in any compliant zstd decoder, including the native C library.
 */
export async function compress(
  data: Uint8Array,
  level: number = DEFAULT_LEVEL,
): Promise<Uint8Array> {
  loading ??= load();
  return (await loading).compress(data, level);
}

/**
 * Decompress a complete Zstandard frame. Rejects if the input is not a valid,
 * complete frame.
 */
export async function decompress(data: Uint8Array): Promise<Uint8Array> {
  loading ??= load();
  return (await loading).decompress(data);
}
