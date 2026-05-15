# structured-zstd

**Pure-Rust Zstandard codec with a production-grade decoder, dictionary handle reuse, and an actively-improved encoder. Builds with plain `cargo` — no cmake, no system zstd, no FFI. `no_std` ready for embedded.**

[![CI](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml/badge.svg)](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/structured-zstd.svg)](https://crates.io/crates/structured-zstd)
[![docs.rs](https://docs.rs/structured-zstd/badge.svg)](https://docs.rs/structured-zstd)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/structured-world/structured-zstd/blob/main/LICENSE)

## Quick start

```toml
[dependencies]
structured-zstd = "0.0.20"
```

```rust
use structured_zstd::encoding::{compress_to_vec, CompressionLevel};

let compressed = compress_to_vec(&b"hello world"[..], CompressionLevel::from_level(7));
```

For `no_std` builds disable the default features:

```toml
[dependencies]
structured-zstd = { version = "0.0.20", default-features = false }
```

## Status

### Decoder — production-ready

Complete [RFC 8878](https://www.rfc-editor.org/rfc/rfc8878) implementation, including dictionary-backed streams, raw / RLE / compressed blocks, and the full Zstandard frame format with optional content checksums.

### Encoder — full level range, active parity work

All standard compression levels are wired and produce valid Zstandard frames decodable by both this crate and upstream C zstd:

- **Named presets:** `Fastest` (≈1), `Default` (≈3), `Better` (≈7), `Best` (≈11)
- **Numeric levels:** `0..=22` and negative ultra-fast levels via `CompressionLevel::from_level(n)` — C zstd-compatible numbering
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

| Level range | Backend |
|-------------|---------|
| 1           | `Simple` matcher |
| 2-3         | `Dfast` |
| 4           | `Row` matcher |
| 5-15        | `HashChain` with lazy / lazy2 tuning |
| 16-17       | `btopt`-style price parser on top of hash-chain candidates |
| 18-19       | `btultra`-style price parser profile |
| 20-22       | `btultra2`-style dual-profile pass (choose lower-cost parse) |

The `greedy` family is not implemented as a dedicated strategy yet — its target ratios are covered by adjacent levels.

</details>

## Performance

Per-merge benchmarks publish to GitHub Pages: **[structured-world.github.io/structured-zstd/dev/bench](https://structured-world.github.io/structured-zstd/dev/bench/)**.

The CI matrix covers `x86_64-linux-gnu`, `i686-linux-gnu`, and `x86_64-musl`; the dashboard exposes per-target / stage / scenario / level filtering. The encoder architecture rewrite ([#111](https://github.com/structured-world/structured-zstd/issues/111)) is the active surface for compression-side work; the public benchmark report tracks the delta vs upstream C zstd over time.

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

## Project relationship

Maintained fork of [KillingSpark/zstd-rs](https://github.com/KillingSpark/zstd-rs) (ruzstd) by the [Structured World Foundation](https://sw.foundation). We sync periodically with upstream but maintain an independent development trajectory focused on the [CoordiNode](https://github.com/structured-world/coordinode) database engine's per-label dictionary needs.

## Support the project

<div align="center">

![USDT TRC-20 Donation QR Code](https://raw.githubusercontent.com/structured-world/structured-zstd/main/assets/usdt-qr.svg)

USDT (TRC-20): `TFDsezHa1cBkoeZT5q2T49Wp66K8t2DmdA`

</div>

## License

Apache License 2.0. Contributions will be published under the same Apache 2.0 license.
