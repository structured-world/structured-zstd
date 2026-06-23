# structured-zstd

**Pure-Rust Zstandard codec with a production-grade decoder, dictionary handle reuse, and an actively-improved encoder. Builds with plain `cargo` — no cmake, no system zstd, no FFI. `no_std` ready for embedded.**

[![CI](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml/badge.svg)](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/structured-zstd.svg)](https://crates.io/crates/structured-zstd)
[![docs.rs](https://docs.rs/structured-zstd/badge.svg)](https://docs.rs/structured-zstd)
[![npm downloads](https://img.shields.io/npm/dw/%40structured-world%2Fstructured-zstd?label=npm%20downloads)](https://www.npmjs.com/package/@structured-world/structured-zstd)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

## Quick start

```bash
cargo add structured-zstd
```

```rust
use structured_zstd::encoding::{compress_to_vec, CompressionLevel};

let compressed = compress_to_vec(&b"hello world"[..], CompressionLevel::from_level(7));
```

For `no_std` builds disable the default features:

```bash
cargo add structured-zstd --no-default-features
```

The decoder ships per-CPU-tier SIMD kernels, each behind a cargo feature
(the tier is picked at runtime with `std`, or at compile time from
`target_feature` on `no_std`): `kernel_scalar`, `kernel_sse2`,
`kernel_bmi2`, `kernel_avx2`, `kernel_vbmi2` (x86) and `kernel_neon`,
`kernel_sve` (aarch64). All are on by default **except `kernel_vbmi2`**
(AVX-512): on AVX-512 hosts that tier is measurably slower than `kernel_avx2`
for this decode workload — AVX-512's license-based frequency downclocking
stalls the surrounding (bursty, memory-bound) code, and the heavier kernel
never amortizes — so by default the runtime dispatch is capped at AVX2 and
AVX-512 hosts fall back to the (faster) AVX2 tier. The kernel is still shipped
and can be opted in with `--features kernel_vbmi2` for a sustained AVX-512
workload that genuinely benefits. The scalar kernel is always compiled (it is the
mandatory fallback), so `kernel_scalar` is a marker that gates no code;
disabling the SIMD tiers is what trims the binary. A scalar-only build —
`--no-default-features` (or, equivalently, naming the marker explicitly) —
compiles out the per-tier SIMD kernel dispatch, its BMI2/AVX2/VBMI2/NEON
trampolines, and the explicit SSE2/NEON intrinsics in the small fixed-size
copy primitives — all gated on the matching `kernel_*` feature. These features
control the crate's own explicit SIMD only; the compiler's autovectorizer may
still emit vector instructions from ordinary scalar code regardless:

```bash
cargo add structured-zstd --no-default-features --features kernel_scalar
```

Release notes for every version live in [`zstd/CHANGELOG.md`](https://github.com/structured-world/structured-zstd/blob/main/zstd/CHANGELOG.md) (maintained by [release-plz](https://release-plz.dev/)).

## Status

### Decoder — production-ready

Complete [RFC 8878](https://www.rfc-editor.org/rfc/rfc8878) implementation, including dictionary-backed streams, raw / RLE / compressed blocks, and the full Zstandard frame format with optional content checksums.

### Encoder — full level range, active parity work

All standard compression levels are wired and produce valid Zstandard frames decodable by both this crate and upstream C zstd:

- **Named presets:** `Fastest` (≈1), `Default` (≈3), `Better` (≈7), `Best` (≈11)
- **Numeric levels:** `0..=22` and negative ultra-fast levels via `CompressionLevel::from_level(n)` — C zstd-compatible numbering
- **Fine-grained parameters:** override individual knobs (`windowLog`, `hashLog`, `chainLog`, `searchLog`, `minMatch`, `targetLength`, `strategy`) and activate **long-distance matching** via `CompressionParameters::builder(...)`, the drop-in equivalent of C zstd's `ZSTD_CCtx_setParameter` surface
- **Streaming encoder** via `std::io::Write`
- **Dictionary compression** with the same dictionary format C zstd consumes
- **Frame Content Size** — `FrameCompressor` writes FCS automatically; `StreamingEncoder` requires `set_pledged_content_size()` before the first write
- **Content checksums** opt-in

The encoder is undergoing an architectural rewrite — see [#111](https://github.com/structured-world/structured-zstd/issues/111) for the roadmap.

### Dictionary training

Behind the `dict_builder` feature flag, the `dictionary` module can:

- build raw dictionaries with COVER (`create_raw_dict_from_source`)
- build raw dictionaries with FastCOVER (`create_fastcover_raw_dict_from_source`)
- finalize raw content into the full zstd dictionary format (`finalize_raw_dict`)
- train + finalize in one pure-Rust flow (`create_fastcover_dict_from_source`)

<details>
<summary>Internal: compression strategy backends</summary>

| Level range | Strategy | Backend |
|-------------|----------|---------|
| 1-2         | `Fast`     | `Simple` matcher |
| 3-4         | `Dfast`    | `Dfast` two-tier hash |
| 5           | `Greedy`   | `Row` matcher (`lazy_depth=0`) |
| 6-15        | `Lazy` / `Lazy2` | `HashChain` (`lazy_depth=1` or `2`) |
| 16-17       | `BtOpt`    | `HashChain` candidates + `btopt` price parser |
| 18          | `BtUltra`  | `HashChain` candidates + `btultra` price parser |
| 19-22       | `BtUltra2` | `HashChain` candidates + `btultra2` dual-profile parse |

The level → strategy column matches upstream zstd `ZSTD_defaultCParameters[0]` at `zstd/lib/compress/clevels.h:25-50` (srcSize > 256 KiB tier). Upstream routes `greedy`/`lazy`/`lazy2` through its row-based matchfinder when `windowLog > 14`; we route `Greedy` through the row matcher (matching upstream) but `Lazy`/`Lazy2` through the hash-chain matcher — an intentional architectural difference, not an oversight.

</details>

## Performance

Per-merge benchmarks publish to GitHub Pages: **[structured-world.github.io/structured-zstd/dev/bench](https://structured-world.github.io/structured-zstd/dev/bench/)**.

The CI matrix covers `x86_64-linux-gnu`, `i686-linux-gnu`, and `x86_64-musl`; the dashboard exposes per-target / stage / scenario / level filtering. The encoder architecture rewrite ([#111](https://github.com/structured-world/structured-zstd/issues/111)) is the active surface for compression-side work; the public benchmark report tracks the delta vs upstream C zstd over time. A dedicated dashboard section also tracks the WebAssembly build (`simd128` + `scalar`) against the most popular npm wasm zstd, [`@bokuweb/zstd-wasm`](https://www.npmjs.com/package/@bokuweb/zstd-wasm), over time.

See [BENCHMARKS.md](https://github.com/structured-world/structured-zstd/blob/main/BENCHMARKS.md) for the methodology — small payloads, entropy extremes, a `100 MiB` large-stream scenario, repository corpus fixtures, and optional local Silesia corpora.

## Usage

### Compression

```rust
use structured_zstd::encoding::{compress, compress_to_vec, CompressionLevel};

let data: &[u8] = b"hello world";
// Named level
let compressed = compress_to_vec(data, CompressionLevel::Fastest);
// Numeric level (C zstd compatible: 0 = default, 1-22, negative for ultra-fast)
let compressed = compress_to_vec(data, CompressionLevel::from_level(7));
```

```rust,no_run
use structured_zstd::encoding::{CompressionLevel, StreamingEncoder};
use std::io::Write;

let mut out = Vec::new();
let mut encoder = StreamingEncoder::new(&mut out, CompressionLevel::Fastest);
encoder.write_all(b"hello ")?;
encoder.write_all(b"world")?;
encoder.finish()?;
# Ok::<(), std::io::Error>(())
```

#### Fine-grained parameters

Override individual compression knobs (the drop-in equivalent of C zstd's
`ZSTD_CCtx_setParameter`). Every knob left unset inherits the base level's
default, so a parameter set that overrides nothing reproduces plain
level-based compression. Long-distance matching is off at every level preset
and is activated only here:

```rust
use structured_zstd::encoding::{
    compress_with_parameters, CompressionLevel, CompressionParameters, Strategy,
};

let data: &[u8] = b"hello world";
let params = CompressionParameters::builder(CompressionLevel::Level(19))
    .window_log(22)
    .strategy(Strategy::Btultra2)
    .enable_long_distance_matching(true)
    .build()
    .expect("parameters within bounds");

let compressed = compress_with_parameters(data, &params);
```

Each parameter's valid range is queryable via `CParameter::bounds()` (the
analogue of `ZSTD_cParam_getBounds`); the builder validates every set knob.

### Decompression

```rust,no_run
use structured_zstd::decoding::StreamingDecoder;
use structured_zstd::io::Read;

let compressed_data: Vec<u8> = vec![];
let mut source: &[u8] = &compressed_data;
let mut decoder = StreamingDecoder::new(&mut source).unwrap();

let mut result = Vec::new();
decoder.read_to_end(&mut result).unwrap();
```

### Dictionary-backed decompression

```rust,no_run
use structured_zstd::decoding::{DictionaryHandle, FrameDecoder, StreamingDecoder};
use structured_zstd::io::Read;

let compressed: Vec<u8> = vec![];
let dict_bytes: Vec<u8> = vec![];
let mut output = vec![0u8; 1024];

// Parse dictionary once, then reuse handle.
let handle = DictionaryHandle::decode_dict(&dict_bytes).unwrap();
let mut decoder = FrameDecoder::new();
let _written = decoder
    .decode_all_with_dict_handle(compressed.as_slice(), &mut output, &handle)
    .unwrap();

// Compatibility path: pass raw dictionary bytes directly.
let mut decoder = FrameDecoder::new();
let _written = decoder
    .decode_all_with_dict_bytes(compressed.as_slice(), &mut output, &dict_bytes)
    .unwrap();

// Streaming helpers exist for both handle- and bytes-based paths.
let mut source: &[u8] = &compressed;
let mut stream = StreamingDecoder::new_with_dictionary_handle(&mut source, &handle).unwrap();
let mut sink = Vec::new();
stream.read_to_end(&mut sink).unwrap();
```

## Storage-format extensions

Behind the `lsm` Cargo feature (default **off**), `structured-zstd`
exposes a typed `SkippableFrame` API
(`structured_zstd::skippable`) for storage-format authors who need
to interleave application metadata with zstd data, plus a
block-subset partial decoder: `FrameDecoder::decode_blocks_partial(src,
start_block, end_block, resume, emit_resume)` decodes only the inner
blocks covering a requested range (skipping the trailing ones) and
preserves the clean prefix on a corrupt block, while
`FrameEmitInfo::decompressed_byte_range(block_index)` returns the
decompressed byte range of a given block, so a range query can locate
which inner blocks cover a target byte window. For incremental /
resumable decoding, pass `emit_resume = true` to capture a `ResumeState`
(cross-block entropy tables + repcode history + next-block coordinates)
in `PartialDecode::resume_state`, then feed it back via the `resume`
argument (`ResumeInput { window_prime, state }`) to continue from a later
block WITHOUT re-decompressing the prefix — even across a dropped (cold)
decoder. Enable on the command line:

```bash
cargo add structured-zstd --features lsm
```

or in `Cargo.toml`:

```toml
[dependencies]
structured-zstd = { version = "0", features = ["lsm"] }
```

The ecosystem registry of allocated skippable-frame magic variants
and the allocation policy live in
[docs/SKIPPABLE_MAGIC_ALLOCATIONS.md](https://github.com/structured-world/structured-zstd/blob/main/docs/SKIPPABLE_MAGIC_ALLOCATIONS.md).
<!-- Absolute URL is intentional: this README is embedded into the
crate's rustdoc via `#![doc = include_str!("../README.md")]` in
zstd/src/lib.rs, where relative paths resolve under docs.rs and 404.
The registry is also the canonical single source of truth on
upstream `main`, so the link target is correct for forks too —
fork consumers should point readers at the upstream registry
rather than maintain divergent copies. -->

## WebAssembly / npm

JavaScript / TypeScript consumers can use the codec from npm — no native
addons, no build step:

```sh
npm install @structured-world/structured-zstd
```

```ts
import { compress, decompress } from "@structured-world/structured-zstd";
const framed = await compress(new TextEncoder().encode("hello"), 19);
const plain = await decompress(framed);
```

The package ships two WebAssembly payloads — one built with the `simd128`
SIMD tier, one scalar — and selects the fast one at runtime from the host
engine's capabilities. Pure ESM, strict TypeScript types. Frames interoperate
with native zstd. Source lives in
[`zstd-wasm/`](https://github.com/structured-world/structured-zstd/tree/main/zstd-wasm);
see the
[package README](https://github.com/structured-world/structured-zstd/blob/main/zstd-wasm/npm/README.md).

## Project relationship

Maintained fork of [KillingSpark/zstd-rs](https://github.com/KillingSpark/zstd-rs) (ruzstd) by the [Structured World Foundation](https://sw.foundation). We sync periodically with upstream but maintain an independent development trajectory focused on the [CoordiNode](https://github.com/structured-world/coordinode) database engine's per-label dictionary needs.

## Support the project

<div align="center">

![USDT TRC-20 Donation QR Code](https://raw.githubusercontent.com/structured-world/structured-zstd/main/assets/usdt-qr.svg)

USDT (TRC-20): `TFDsezHa1cBkoeZT5q2T49Wp66K8t2DmdA`

</div>

## License

Apache License 2.0. Contributions will be published under the same Apache 2.0 license.
