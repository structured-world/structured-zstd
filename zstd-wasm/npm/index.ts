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
  ZstdDecompressStream: (new (checksum?: ContentChecksum) => DecompressStream) & {
    withDictionary: (dict: Uint8Array, checksum?: ContentChecksum) => DecompressStream;
    withPreparedDictionary: (
      dict: WasmDictionary,
      checksum?: ContentChecksum,
    ) => DecompressStream;
  };
  ZstdCompressStream: (new (level: number, checksum?: boolean) => CompressStream) & {
    withDictionary: (
      level: number,
      dict: Uint8Array,
      checksum?: boolean,
    ) => CompressStream;
    withPreparedDictionary: (
      level: number,
      dict: WasmDictionary,
      checksum?: boolean,
    ) => CompressStream;
  };
  ZstdDictionary: new (dict: Uint8Array) => WasmDictionary;
}

/** Raw wasm-side prepared-dictionary handle (see {@link ZstdDict}). */
interface WasmDictionary {
  readonly id: number;
  compress(data: Uint8Array, level: number, checksum?: boolean): Uint8Array;
  decompress(data: Uint8Array, checksum?: ContentChecksum): Uint8Array;
  free(): void;
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
 * (final block, plus the 4-byte XXH64 trailer only if the stream was created
 * with `checksum` enabled). Peak memory is O(window), not O(input) — the frame
 * is emitted block-by-block instead of buffered whole. The frame omits
 * `Frame_Content_Size` (unknown while streaming) yet decodes in any compliant
 * zstd decoder. Call {@link CompressStream.free} when done.
 */
export interface CompressStream {
  /** Feed plaintext; returns compressed bytes complete so far (may be empty). */
  push(chunk: Uint8Array): Uint8Array;
  /**
   * Seal the frame; returns the final block, followed by the 4-byte XXH64
   * content-checksum trailer only when the stream was created with `checksum`
   * enabled (otherwise just the final block, no trailer).
   */
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
 *
 * @param checksum Defaults to `false` (no trailing checksum, matching
 * libzstd's `ZSTD_c_checksumFlag = 0`); pass `true` to append the XXH64
 * content checksum.
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
 *
 * @param checksum Defaults to `false` (no trailing checksum, matching
 * libzstd's `ZSTD_c_checksumFlag = 0`); pass `true` to append the XXH64
 * content checksum.
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
 *
 * @param checksum Defaults to {@link ContentChecksum.None} (skip the XXH64
 * pass); {@link ContentChecksum.Verify} rejects on a checksum mismatch.
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
 *
 * @param checksum Applies to the whole stream (set at construction). Defaults
 * to {@link ContentChecksum.None}; {@link ContentChecksum.Verify} validates the
 * content checksum at `finish()`.
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
 *
 * @param checksum Defaults to `false` (no trailing checksum, matching
 * libzstd's `ZSTD_c_checksumFlag = 0`); pass `true` to seal the frame with a
 * trailing XXH64 content checksum.
 */
export async function createCompressStream(
  level: number = DEFAULT_LEVEL,
  checksum?: boolean,
): Promise<CompressStream> {
  loading ??= load();
  return new (await loading).ZstdCompressStream(level, checksum);
}

/**
 * Create an incremental streaming compressor seeded with a raw zstd `dict`
 * (e.g. from `zstd --train`), at `level`. Mirrors `ZSTD_CCtx_loadDictionary` on
 * a streaming context: the dictionary primes the matcher and the first block's
 * entropy, so small / similar payloads compress far better. Decode the produced
 * frames with {@link createDecompressStreamWithDictionary} (same `dict`) or the
 * one-shot {@link decompressUsingDict}. Rejects if the dictionary is invalid.
 *
 * @param checksum Defaults to `false`; pass `true` for a trailing XXH64 checksum.
 */
export async function createCompressStreamWithDictionary(
  dict: Uint8Array,
  level: number = DEFAULT_LEVEL,
  checksum?: boolean,
): Promise<CompressStream> {
  loading ??= load();
  return (await loading).ZstdCompressStream.withDictionary(level, dict, checksum);
}

/**
 * Create an incremental streaming decompressor primed with a raw zstd `dict`.
 * `dict` must be the same dictionary the frame was compressed with (e.g. via
 * {@link createCompressStreamWithDictionary}). Mirrors
 * `ZSTD_DCtx_loadDictionary`. Rejects if the dictionary is malformed.
 *
 * @param checksum Applies to the whole stream; see {@link createDecompressStream}.
 */
export async function createDecompressStreamWithDictionary(
  dict: Uint8Array,
  checksum?: ContentChecksum,
): Promise<DecompressStream> {
  loading ??= load();
  return (await loading).ZstdDecompressStream.withDictionary(dict, checksum);
}

/**
 * A dictionary prepared once and reused across many compressions,
 * decompressions, and streams — the recommended way to use dictionaries when
 * more than one payload is involved.
 *
 * Construction parses the dictionary a single time; afterwards
 * {@link ZstdDict.compress} reuses a primed encoder (the per-frame dictionary
 * setup cost disappears), {@link ZstdDict.decompress} reuses one decoder
 * workspace, and the stream factories seed from the prepared tables instead of
 * re-parsing the blob. Raw content (no dictionary magic) is accepted; such
 * dictionaries have `id === 0` and produced frames carry no dictionary ID.
 *
 * Call {@link ZstdDict.free} when done to release the wasm-side memory
 * eagerly (it is otherwise reclaimed with the object).
 *
 * ```ts
 * const dict = await ZstdDict.create(dictBytes);
 * const frame = dict.compress(payload, 19);
 * const back = dict.decompress(frame);
 * const stream = dict.compressStream(19);
 * ```
 */
export class ZstdDict {
  /** @internal */
  private constructor(
    private readonly inner: WasmDictionary,
    private readonly module: Payload,
  ) {}

  /** Parse `dict` once for repeated use. Rejects if a magic-prefixed blob is corrupt. */
  static async create(dict: Uint8Array): Promise<ZstdDict> {
    loading ??= load();
    const module = await loading;
    return new ZstdDict(new module.ZstdDictionary(dict), module);
  }

  /** The dictionary ID (0 for raw content). */
  get id(): number {
    return this.inner.id;
  }

  /**
   * Compress `data` at `level` (defaults to {@link DEFAULT_LEVEL}). The first
   * call at a given level primes the encoder; following calls reuse the primed
   * state — the hot path for many small frames against one dictionary.
   */
  compress(data: Uint8Array, level: number = DEFAULT_LEVEL, checksum?: boolean): Uint8Array {
    return this.inner.compress(data, level, checksum);
  }

  /** Decompress a frame produced with this dictionary. */
  decompress(data: Uint8Array, checksum?: ContentChecksum): Uint8Array {
    return this.inner.decompress(data, checksum);
  }

  /**
   * Open a streaming compressor seeded from this prepared dictionary (no
   * per-stream re-parse). Same contract as {@link createCompressStream}.
   */
  compressStream(level: number = DEFAULT_LEVEL, checksum?: boolean): CompressStream {
    return this.module.ZstdCompressStream.withPreparedDictionary(level, this.inner, checksum);
  }

  /**
   * Open a streaming decompressor against this prepared dictionary (per-stream
   * setup is a reference-count bump). Also decodes frames whose headers omit
   * the dictionary ID. Same contract as {@link createDecompressStream}.
   */
  decompressStream(checksum?: ContentChecksum): DecompressStream {
    return this.module.ZstdDecompressStream.withPreparedDictionary(this.inner, checksum);
  }

  /** Release the wasm-side dictionary memory eagerly. */
  free(): void {
    this.inner.free();
  }
}
