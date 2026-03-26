# structured-zstd

Pure Rust zstd implementation — managed fork of [ruzstd](https://github.com/KillingSpark/zstd-rs).

[![CI](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml/badge.svg)](https://github.com/structured-world/structured-zstd/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/structured-zstd.svg)](https://crates.io/crates/structured-zstd)
[![docs.rs](https://docs.rs/structured-zstd/badge.svg)](https://docs.rs/structured-zstd)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

## Managed Fork

This is a **maintained fork** of [KillingSpark/zstd-rs](https://github.com/KillingSpark/zstd-rs) (ruzstd) by [Structured World Foundation](https://sw.foundation). We maintain additional features and hardening for the [CoordiNode](https://github.com/structured-world/coordinode) database engine.

**Fork goals:**
- Dictionary compression improvements (critical for per-label trained dictionaries in LSM-tree)
- Performance parity with C zstd for decompression (currently 1.4-3.5x slower)
- Additional compression levels (Default/Better/Best — currently only Fastest is implemented)
- No FFI — pure `cargo build`, no cmake/system libraries (ADR-013 compliance)

**Upstream relationship:** We periodically sync with upstream but maintain an independent development trajectory focused on CoordiNode requirements.

## What is this

A pure Rust implementation of the Zstandard compression format, as defined in [RFC 8878](https://www.rfc-editor.org/rfc/rfc8878.pdf).

This crate contains a fully operational decompressor and a compressor that is usable but does not yet match the speed, ratio, or configurability of the original C library.

## Current Status

### Decompression

Complete RFC 8878 implementation. Performance: ~1.4-3.5x slower than C zstd depending on data compressibility.

### Compression

- [x] Uncompressed blocks
- [x] Fastest (roughly level 1)
- [ ] Default (roughly level 3)
- [ ] Better (roughly level 7)
- [ ] Best (roughly level 11)
- [x] Checksums
- [ ] Dictionary compression

### Dictionary Generation

When the `dict_builder` feature is enabled, the `dictionary` module can create raw content dictionaries. Within 0.2% of the official implementation on the `github-users` sample set.

## Usage

### Compression

```rust
use structured_zstd::encoding::{compress, compress_to_vec, CompressionLevel};

let data: &[u8] = b"hello world";
let compressed = compress_to_vec(data, CompressionLevel::Fastest);
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

## Support the Project

<div align="center">

![USDT TRC-20 Donation QR Code](https://raw.githubusercontent.com/structured-world/structured-zstd/main/assets/usdt-qr.svg)

USDT (TRC-20): `TFDsezHa1cBkoeZT5q2T49Wp66K8t2DmdA`

</div>

## License

Apache License 2.0

Contributions will be published under the same Apache 2.0 license.
