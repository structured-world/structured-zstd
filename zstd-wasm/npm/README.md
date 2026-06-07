# @structured-world/structured-zstd

Pure-Rust [Zstandard](https://facebook.github.io/zstd/) (zstd) codec compiled
to WebAssembly. Compress and decompress in the **browser, Node.js and Deno** —
no native addons, no FFI, no build step for consumers.

- **Automatic SIMD.** Ships two payloads — one built with the WebAssembly
  `simd128` SIMD tier, one scalar — and picks the fast one at runtime via
  engine feature detection. You import one package; the right `.wasm` loads.
- **Pure ESM, strict TypeScript.** First-class types, tree-shakeable, `type:
  module`.
- **Interoperable.** Frames produced here decode in the native C `zstd`, and
  frames from the C library decode here.

## Install

```sh
npm install @structured-world/structured-zstd
```

## Usage

```ts
import { compress, decompress } from "@structured-world/structured-zstd";

const data = new TextEncoder().encode("hello, zstd ".repeat(1000));

// `level` follows the zstd scale: 1..=22 (higher = smaller/slower),
// negatives (-7..=-1) for the ultra-fast tier. Defaults to 3.
const framed = await compress(data, 19);
const plain = await decompress(framed);

console.assert(plain.length === data.length);
```

The codec initialises lazily on first call. To pre-warm it (e.g. before a
latency-sensitive path), call `init()`:

```ts
import { init, compress } from "@structured-world/structured-zstd";

await init(); // selects + instantiates the best payload up front
const out = await compress(payload);
```

## API

| Export | Signature | Notes |
|--------|-----------|-------|
| `compress` | `(data: Uint8Array, level?: number) => Promise<Uint8Array>` | `level` defaults to `3`. |
| `decompress` | `(data: Uint8Array) => Promise<Uint8Array>` | Rejects on a malformed/incomplete frame. |
| `compressUsingDict` | `(data: Uint8Array, dict: Uint8Array, level?: number) => Promise<Uint8Array>` | Dictionary compression (raw zstd dictionary, e.g. `zstd --train`). |
| `decompressUsingDict` | `(data: Uint8Array, dict: Uint8Array) => Promise<Uint8Array>` | `dict` must match the one used to compress. |
| `createDecompressStream` | `() => Promise<DecompressStream>` | Incremental streaming decoder (see below). |
| `init` | `() => Promise<void>` | Optional pre-warm; idempotent. |

```ts
import { compressUsingDict, decompressUsingDict } from "@structured-world/structured-zstd";
const framed = await compressUsingDict(record, dictionary, 19);
const back = await decompressUsingDict(framed, dictionary);
```

### Streaming decompression

Decode a frame incrementally without buffering all of it — feed compressed
chunks (e.g. from a `fetch` body) and read output as it arrives:

```ts
import { createDecompressStream } from "@structured-world/structured-zstd";

const stream = await createDecompressStream();
const out: Uint8Array[] = [];
for await (const chunk of response.body) out.push(stream.push(chunk));
out.push(stream.finish()); // throws if the stream ended mid-frame
stream.free();             // release the wasm handle
```

`push(chunk)` returns whatever output is available so far (possibly empty
while a block is still incomplete); the decoder window is held wasm-side
across chunks.

## How the payload is selected

WebAssembly has no runtime CPU detection, so SIMD selection is an engine
**capability** probe ([`wasm-feature-detect`](https://www.npmjs.com/package/wasm-feature-detect)
`simd()`): when the host supports wasm SIMD the `simd128` payload loads,
otherwise the scalar one. In Node.js the `.wasm` bytes are read from disk; in
the browser/Deno they are fetched relative to the module.

## License

[Apache-2.0](./LICENSE) © Structured World Foundation. Built from
[`structured-zstd`](https://github.com/structured-world/structured-zstd).
