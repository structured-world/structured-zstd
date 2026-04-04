# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.6](https://github.com/structured-world/structured-zstd/compare/v0.0.5...v0.0.6) - 2026-04-04

### Added

- *(encoding)* write frame content size in encoder output ([#60](https://github.com/structured-world/structured-zstd/pull/60))

### Performance

- *(decoding)* pre-allocate decode buffer from sequence block analysis ([#59](https://github.com/structured-world/structured-zstd/pull/59))
- *(decoding)* branchless bitstream reader with mask table and BMI2 support ([#58](https://github.com/structured-world/structured-zstd/pull/58))
- *(decoding)* dual-state interleaved FSE sequence decoding ([#55](https://github.com/structured-world/structured-zstd/pull/55))

## [0.0.5](https://github.com/structured-world/structured-zstd/compare/v0.0.4...v0.0.5) - 2026-04-03

### Added

- *(encoding)* add Best compression level (zstd level 11, btlazy2 strategy) ([#53](https://github.com/structured-world/structured-zstd/pull/53))
- *(encoding)* add Better compression level (zstd level 7, lazy2 strategy) ([#48](https://github.com/structured-world/structured-zstd/pull/48))

### Performance

- *(decoding)* 4-stream interleaved Huffman decode and bulk table init ([#54](https://github.com/structured-world/structured-zstd/pull/54))

## [0.0.4](https://github.com/structured-world/structured-zstd/compare/v0.0.3...v0.0.4) - 2026-04-01

### Added

- *(encoding)* add streaming write encoder ([#45](https://github.com/structured-world/structured-zstd/pull/45))
- *(encoding)* add dictionary compression support ([#44](https://github.com/structured-world/structured-zstd/pull/44))

### Performance

- *(decoding)* optimize sequence execution with overlap fast paths ([#42](https://github.com/structured-world/structured-zstd/pull/42))
- *(encoding)* interleave fastest hash fill insertion ([#41](https://github.com/structured-world/structured-zstd/pull/41))
- *(encoding)* align fastest matcher with zstd fast path ([#39](https://github.com/structured-world/structured-zstd/pull/39))

### Testing

- *(bench)* expand benchmark parity matrix ([#43](https://github.com/structured-world/structured-zstd/pull/43))
- *(bench)* expand zstd benchmark suite ([#38](https://github.com/structured-world/structured-zstd/pull/38))

## [0.0.3](https://github.com/structured-world/structured-zstd/compare/v0.0.2...v0.0.3) - 2026-03-26

### Added

- *(encoder)* FSE table reuse and offset history optimization ([#33](https://github.com/structured-world/structured-zstd/pull/33))
- large literals block support (>262KB) ([#30](https://github.com/structured-world/structured-zstd/pull/30))

### Fixed

- *(encoding)* implement default compression level ([#34](https://github.com/structured-world/structured-zstd/pull/34))
- use local Readme.md for crate readme

## [0.0.2](https://github.com/structured-world/structured-zstd/compare/v0.0.1...v0.0.2) - 2026-03-25

### Added

- managed fork setup — README, FUNDING, crate rename to structured-zstd ([#2](https://github.com/structured-world/structured-zstd/pull/2))
