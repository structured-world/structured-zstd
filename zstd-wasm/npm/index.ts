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

/**
 * How the decoder treats a frame's optional XXH64 content checksum. Numeric
 * values match the wasm-bindgen `ContentChecksum` enum.
 */
export enum ContentChecksum {
  /** Skip the XXH64 pass entirely (fastest; no verification). */
  None = 0,
  /** Compute the checksum and expose it via accessors; does not error on a mismatch. */
  EmitOnly = 1,
  /** Compute and verify; a mismatch rejects the decode. */
  Verify = 2,
}

/** Shape of a wasm-pack `web`-target payload glue module (simd or scalar). */
interface Payload {
  /** Async wasm initialiser. Accepts wasm bytes / a URL, or nothing (browser). */
  // Typed `unknown`: wasm-bindgen's `web` init accepts the broad `InitInput`
  // (bytes / URL / Response / Module), and Node's `fs.readFile` returns
  // `Buffer<ArrayBufferLike>`, wider than the DOM `BufferSource` in the .d.ts.
  default: (moduleOrPath?: unknown) => Promise<unknown>;
  compress: (data: Uint8Array, level: number, checksum?: boolean) => Uint8Array;
  decompress: (data: Uint8Array, checksum?: ContentChecksum) => Uint8Array;
  compressUsingDict: (
    data: Uint8Array,
    dict: Uint8Array,
    level: number,
    checksum?: boolean,
  ) => Uint8Array;
  decompressUsingDict: (
    data: Uint8Array,
    dict: Uint8Array,
    checksum?: ContentChecksum,
  ) => Uint8Array;
  ZstdDecompressStream: new (checksum?: ContentChecksum) => DecompressStream;
  ZstdCompressStream: new (level: number, checksum?: boolean) => CompressStream;
}

/**
 * Incremental streaming decompressor handle. Feed compressed chunks with
 * {@link DecompressStream.push} and read decompressed output as it becomes
 * available; call {@link DecompressStream.finish} at end of input. The decoder
 * window is held on the wasm side across chunks, so a large frame never needs
 * to be fully buffered. Call {@link DecompressStream.free} when done.
 */
export interface DecompressStream {
  /** Feed compressed bytes; returns decompressed output available so far. */
  push(chunk: Uint8Array): Uint8Array;
  /**
   * Signal end of input; returns the final bytes. Throws if incomplete, or
   * (in {@link ContentChecksum.Verify} mode) if the content checksum is wrong.
   */
  finish(): Uint8Array;
  /**
   * Content checksum stored in the frame trailer, or `undefined` if none.
   * Meaningful after {@link DecompressStream.finish}.
   */
  storedChecksum(): number | undefined;
  /**
   * XXH64 digest computed over the output (low 32 bits), or `undefined` under
   * {@link ContentChecksum.None} or when the frame carried no checksum. Lets
   * callers verify manually under {@link ContentChecksum.EmitOnly}.
   */
  calculatedChecksum(): number | undefined;
  /** Release the underlying wasm handle. */
  free(): void;
}

/**
 * Incremental streaming compressor handle. Feed plaintext chunks with
 * {@link CompressStream.push} and read complete compressed blocks as the
 * matcher window fills; call {@link CompressStream.finish} to seal the frame
 * (final block + checksum). Peak memory is O(window), not O(input) — the frame
 * is emitted block-by-block instead of buffered whole. The frame omits
 * `Frame_Content_Size` (unknown while streaming) yet decodes in any compliant
 * zstd decoder. Call {@link CompressStream.free} when done.
 */
export interface CompressStream {
  /** Feed plaintext; returns compressed bytes complete so far (may be empty). */
  push(chunk: Uint8Array): Uint8Array;
  /** Seal the frame; returns the final block + checksum. */
  finish(): Uint8Array;
  /** Release the underlying wasm handle. */
  free(): void;
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
  checksum?: boolean,
): Promise<Uint8Array> {
  loading ??= load();
  return (await loading).compress(data, level, checksum);
}

/**
 * Decompress a complete Zstandard frame. Rejects if the input is not a valid,
 * complete frame, or — when `checksum` is {@link ContentChecksum.Verify} — if
 * the content checksum does not match. Defaults to
 * {@link ContentChecksum.None} (the XXH64 pass is skipped for speed); pass
 * {@link ContentChecksum.Verify} to validate.
 */
export async function decompress(
  data: Uint8Array,
  checksum?: ContentChecksum,
): Promise<Uint8Array> {
  loading ??= load();
  return (await loading).decompress(data, checksum);
}

/**
 * Compress `data` against a raw Zstandard `dict` (e.g. from `zstd --train`) at
 * `level` (defaults to {@link DEFAULT_LEVEL}). Mirrors C
 * `ZSTD_compress_usingDict` — small, similar payloads compress far better.
 * Rejects if the dictionary is invalid.
 */
export async function compressUsingDict(
  data: Uint8Array,
  dict: Uint8Array,
  level: number = DEFAULT_LEVEL,
  checksum?: boolean,
): Promise<Uint8Array> {
  loading ??= load();
  return (await loading).compressUsingDict(data, dict, level, checksum);
}

/**
 * Decompress a dictionary-encoded frame. `dict` must be the same raw
 * dictionary used to compress it. Mirrors C `ZSTD_decompress_usingDict`.
 * Rejects on a malformed frame or dictionary mismatch.
 */
export async function decompressUsingDict(
  data: Uint8Array,
  dict: Uint8Array,
  checksum?: ContentChecksum,
): Promise<Uint8Array> {
  loading ??= load();
  return (await loading).decompressUsingDict(data, dict, checksum);
}

/**
 * Create an incremental streaming decompressor. Push compressed chunks and
 * read decompressed output as it arrives, then `finish()`; `free()` when done.
 * Unlike the common npm wasm zstd packages, the frame need not be fully
 * buffered — the decoder window lives on the wasm side across chunks.
 */
export async function createDecompressStream(
  checksum?: ContentChecksum,
): Promise<DecompressStream> {
  loading ??= load();
  return new (await loading).ZstdDecompressStream(checksum);
}

/**
 * Create an incremental streaming compressor at `level` (zstd scale: `1..=22`,
 * negatives for the ultra-fast tier; defaults to {@link DEFAULT_LEVEL}). Push
 * plaintext chunks and read compressed blocks as they complete, then
 * `finish()`; `free()` when done. Peak memory is O(window), not O(input) — the
 * frame is emitted block-by-block rather than buffered whole — and the result
 * decodes in any compliant zstd decoder. Symmetric with
 * {@link createDecompressStream}.
 */
export async function createCompressStream(
  level: number = DEFAULT_LEVEL,
  checksum?: boolean,
): Promise<CompressStream> {
  loading ??= load();
  return new (await loading).ZstdCompressStream(level, checksum);
}
