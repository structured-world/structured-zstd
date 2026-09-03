# structured-zstd

**Pure-Rust [Zstandard](https://facebook.github.io/zstd/) (zstd) compression and decompression.** All 22 standard levels plus the negative ultra-fast range, streaming, dictionaries, `no_std` support and a WebAssembly build — with plain `cargo`: no cmake, no system zstd, no FFI.

[![CI](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml/badge.svg)](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/structured-zstd.svg)](https://crates.io/crates/structured-zstd)
[![docs.rs](https://docs.rs/structured-zstd/badge.svg)](https://docs.rs/structured-zstd)
[![npm downloads](https://img.shields.io/npm/dw/%40structured-world%2Fstructured-zstd?label=npm%20downloads)](https://www.npmjs.com/package/@structured-world/structured-zstd)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

## Highlights

- **Production-grade decoder** — complete [RFC 8878](https://www.rfc-editor.org/rfc/rfc8878) implementation: dictionary-backed streams, raw / RLE / compressed blocks, the full frame format, optional content checksums, runtime-dispatched SIMD kernels (SSE2 / BMI2 / AVX2 / NEON, opt-in AVX-512).
- **Full-range encoder** — every C-zstd level (`-131072..=22`) produces valid frames decodable by this crate and by upstream C zstd; named presets, per-knob parameter overrides, long-distance matching, streaming via `std::io::Write`.
- **Dictionaries end to end** — compress and decompress with the same dictionary format C zstd consumes; reusable parsed handles; pure-Rust COVER / FastCOVER training behind the `dict-builder` feature.
- **Wire-compatible both ways** — frames interoperate with C zstd in either direction; interop is enforced in CI against the reference implementation.
- **`no_std` ready** — the decoder builds with `--no-default-features` for embedded and sandboxed targets.
- **WebAssembly / npm** — the same codec as an npm package with automatic SIMD selection; no native addons, no postinstall scripts.
- **Continuously benchmarked** — a public [dashboard](https://structured-world.github.io/structured-zstd/dev/bench/) tracks speed and ratio against C zstd on every merge.

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

Release notes for every version live in [`zstd/CHANGELOG.md`](https://github.com/structured-world/structured-zstd/blob/main/zstd/CHANGELOG.md) (maintained by [release-plz](https://release-plz.dev/)).

## Command-line tool

The tool ships from this same crate, so one name covers both uses:

```bash
cargo add structured-zstd        # the library
cargo install structured-zstd    # the `structured-zstd` binary
```

It carries no dependencies of its own — argument parsing, progress display and
error reporting are written against `std` — so depending on the library pulls
in nothing extra.

The binary speaks the upstream `zstd`
command line: levels (`-1`..`-19`, `--ultra` for `-20`..`-22`, `--fast[=N]`),
`-d`, `-c`, `-o`, `-t`, `-l`, `-D`, `--train`, `-b`, and the usual
`-f`/`-k`/`--rm` file handling.

Flags that only steer how the work is done (`-T`, `-B`, `--adapt`,
`--[no-]progress`, …) are accepted and ignored — their values are still
validated, so a typo is an error rather than silence.
`--target-compressed-block-size` does take effect: it bounds what goes into a
block, so blocks flush sooner. `--long=N` is capped at 27, since a larger
window would produce frames this build's decoder refuses. Flags that would change
the result are refused instead: `--format=` for anything but zstd,
`--patch-from`, `--rsyncable`, `--no-check`, `--[no-]compress-literals`, and
the not-yet-implemented `--pass-through` / `--exclude-compressed`. `-M` is
treated as the safety promise it is: a limit covering the 128 MiB
decompression window plus the decoder's buffers is kept, a tighter one is
refused rather than ignored, and it is refused outright with `--train`, which
it says nothing about.

`--train` and `--train-fastcover` both train with FastCOVER, the algorithm
upstream also defaults to. `--train-cover` and `--train-legacy` name algorithms
this build does not have, so they are refused rather than quietly served by
FastCOVER. `-D` takes either a dictionary produced by `--train` or any file at
all, which is then used as raw content the way upstream does — such a
dictionary has no ID, so the same bytes must be supplied when decoding.

It is deliberately **not** installed as `zstd`, so it never shadows the system
tool. It does dispatch on the name it is invoked under, so linking it as the
familiar names works:

```bash
ln -s "$(command -v structured-zstd)" ~/.local/bin/unzstd    # defaults to -d
ln -s "$(command -v structured-zstd)" ~/.local/bin/zstdcat   # defaults to -d -c
```

Distributions should register it with their alternatives mechanism rather than
overwriting `/usr/bin/zstd`, e.g.

```bash
update-alternatives --install /usr/bin/zstd zstd /usr/bin/structured-zstd 100
```

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

- **Named presets:** `Fastest` (≈1), `Default` (≈3), `Better` (≈7), `Best` (≈13)
- **Frame Content Size:** `FrameCompressor` writes FCS automatically; `StreamingEncoder` requires `set_pledged_content_size()` before the first write
- **Content checksums:** opt-in via `set_content_checksum(true)`

### Fine-grained parameters

Override individual compression knobs (the drop-in equivalent of C zstd's
`ZSTD_CCtx_setParameter`). Every knob left unset inherits the base level's
default, so a parameter set that overrides nothing reproduces plain
level-based compression. Long-distance matching is off at every level preset
and is activated only here; it also needs the (default-on) `ldm` feature.
Without it the builder still accepts `enable_long_distance_matching(true)` and
the frame is still valid, but no long-distance matches are produced:

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

### Dictionaries

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

Compression takes the same dictionary format through
`FrameCompressor::set_dictionary_from_bytes` / `EncoderDictionary::from_bytes`
(one parse, reusable across frames).

Behind the `dict-builder` feature, the `dictionary` module trains dictionaries
in pure Rust:

- COVER (`create_raw_dict_from_source`) and FastCOVER (`create_fastcover_raw_dict_from_source`) raw dictionaries
- `finalize_raw_dict` to produce the full zstd dictionary format
- `create_fastcover_dict_from_source` for train + finalize in one call

## Feature flags

| Feature | Default | What it enables |
|---------|---------|-----------------|
| `std` | ✅ | Runtime CPU detection, `std::io` adapters |
| `hash` | ✅ | XXH64 content checksums |
| `ldm` | ✅ | Long-distance matching (implies `hash`: LDM hashes each window with XXH64) |
| `kernel-sse`, `kernel-bmi2`, `kernel-avx2` | ✅ | x86 SIMD kernels (`kernel-sse` covers both the SSE2 and SSE4.2 tiers) |
| `kernel-neon`, `kernel-sve` | ✅ | aarch64 SIMD kernels |
| `kernel-simd128` | ✅ | WebAssembly SIMD kernel (needs `-C target-feature=+simd128`) |
| `kernel-vbmi2` | ❌ | AVX-512 decode kernel (see note below) |
| `kernel-scalar` | ✅ | Marker for the always-compiled scalar fallback |
| `dict-builder` | ❌ | Pure-Rust COVER / FastCOVER dictionary training |
| `lsm` | ❌ | [Storage-format extensions](#storage-format-extensions) |

Each flag gates its tier wherever that tier exists. `kernel-sse`,
`kernel-bmi2`, `kernel-avx2`, `kernel-neon` and `kernel-simd128` cover both
the decoder and the encoder; `kernel-vbmi2` and `kernel-sve` are decoder-only
(the encoder has no AVX-512 or SVE tier), and `kernel-scalar` gates nothing,
since the scalar path is the mandatory fallback and always compiled. So
`--no-default-features` (optionally with `--features kernel-scalar`) compiles
every per-tier dispatch and all explicit SIMD intrinsics out of the crate.

On x86 and aarch64 with `std`, the tier is picked at runtime from CPU
detection; on `no_std` it comes from the target's `target_feature` set at
compile time. x86 has two 128-bit tiers under `kernel-sse`: SSE4.2 when
available, otherwise a plain-SSE2 tier, so pre-SSE4.2 CPUs still get vector
match compares instead of dropping to scalar.

**WebAssembly is compile-time only**, with or without `std`: wasm has no
runtime feature detection, so both the decoder kernels and the encoder
fastpath additionally require `target_feature = "simd128"`. Building for
`wasm32` with default features and no extra flags therefore stays scalar —
pass `-C target-feature=+simd128` to get the SIMD tier. (The npm package
sidesteps this by shipping separately compiled scalar and `+simd128`
payloads and picking one at load time.)

In every case these features control only the crate's own explicit SIMD; the
compiler's autovectorizer is unaffected.

<details>
<summary>Why AVX-512 is off by default</summary>

On AVX-512 hosts the `kernel-vbmi2` tier measures slower than `kernel-avx2`
for this decode workload: AVX-512's license-based frequency downclocking
stalls the surrounding bursty, memory-bound code and the heavier kernel never
amortizes. By default runtime dispatch is therefore capped at AVX2, and
AVX-512 hosts use the (faster) AVX2 tier. Opt in with
`--features kernel-vbmi2` for a sustained AVX-512 workload that genuinely
benefits.

</details>

## Performance

- Per-merge benchmarks publish to a public dashboard: **[structured-world.github.io/structured-zstd/dev/bench](https://structured-world.github.io/structured-zstd/dev/bench/)** — speed and ratio against upstream C zstd over time.
- The CI matrix covers `x86_64-linux-gnu`, `i686-linux-gnu` and `x86_64-musl`, with per-target / stage / scenario / level filtering on the dashboard.
- A dedicated section tracks the WebAssembly build (`simd128` + scalar) against the most popular npm wasm zstd, [`@bokuweb/zstd-wasm`](https://www.npmjs.com/package/@bokuweb/zstd-wasm).
- Methodology in [BENCHMARKS.md](https://github.com/structured-world/structured-zstd/blob/main/BENCHMARKS.md): small payloads, entropy extremes, a `100 MiB` large-stream scenario, repository corpus fixtures, optional local Silesia corpora.

<details>
<summary>Internal: compression strategy backends</summary>

| Level range | Strategy | Backend |
|-------------|----------|---------|
| 1-2         | `Fast`     | `Simple` matcher |
| 3-4         | `Dfast`    | `Dfast` two-tier hash |
| 5-12        | `Greedy` / `Lazy` / `Lazy2` | `Row` lazy parse (`lazy_depth=0/1/2`): row match-finder above a 2^14 window, hash chain at or below it |
| 13-15       | `Btlazy2`  | `Row` lazy parse over the lazily-sorted binary tree |
| 16-17       | `BtOpt`    | `HashChain` candidates + `btopt` price parser |
| 18          | `BtUltra`  | `HashChain` candidates + `btultra` price parser |
| 19-22       | `BtUltra2` | `HashChain` candidates + `btultra2` dual-profile parse |

The level → strategy column matches upstream zstd `ZSTD_defaultCParameters[0]` at `zstd/lib/compress/clevels.h:25-50` (srcSize > 256 KiB tier); smaller sources shift the row per upstream's size tiers. The whole greedy..btlazy2 band runs upstream's `ZSTD_compressBlock_lazy_generic` parse on the `Row` backend over the three upstream match finders (rows / hash chain per `ZSTD_resolveRowMatchFinderMode`, lazily-sorted binary tree for `btlazy2`).

</details>

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

## Storage-format extensions

Behind the `lsm` feature (default **off**), the crate adds building blocks for
storage-format authors:

- **Skippable frames** — a typed `SkippableFrame` API (`structured_zstd::skippable`) for interleaving application metadata with zstd data.
- **Block-subset partial decode** — `FrameDecoder::decode_blocks_partial` decodes only the inner blocks covering a requested range (skipping the trailing ones) and preserves the clean prefix on a corrupt block.
- **Block-to-byte-range lookup** — `FrameEmitInfo::decompressed_byte_range(block_index)` maps a block to its decompressed byte range, so a range query can locate which blocks cover a target byte window.
- **Resumable decoding** — request a `ResumeState` (cross-block entropy tables + repcode history + next-block coordinates) from a partial decode, then feed it back to continue from a later block, even across a dropped decoder. The state does not carry the match window: the resuming call also supplies the tail of the already-decompressed output (the last `min(window_size, resume_offset)` bytes) via `ResumeInput::window_prime`.

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

## Project relationship

Maintained fork of [KillingSpark/zstd-rs](https://github.com/KillingSpark/zstd-rs) (ruzstd) by the [Structured World Foundation](https://sw.foundation). We sync periodically with upstream but maintain an independent development trajectory focused on the [CoordiNode](https://github.com/structured-world/coordinode) database engine's per-label dictionary needs.

## Support the project

<div align="center">

![USDT TRC-20 Donation QR Code](https://raw.githubusercontent.com/structured-world/structured-zstd/main/assets/usdt-qr.svg)

USDT (TRC-20): `TFDsezHa1cBkoeZT5q2T49Wp66K8t2DmdA`

</div>

## License

Apache License 2.0. Contributions will be published under the same Apache 2.0 license.
