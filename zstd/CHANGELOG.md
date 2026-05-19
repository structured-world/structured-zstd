# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.22](https://github.com/structured-world/structured-zstd/compare/v0.0.21...v0.0.22) - 2026-05-19

### Added

- *(encoder)* strategy-aware literal gates (G4 + G5) ([#182](https://github.com/structured-world/structured-zstd/pull/182))
- *(bench)* #99 Rust↔FFI sequence-stream comparator ([#149](https://github.com/structured-world/structured-zstd/pull/149))

### Documentation

- bump quick-start dep version to 0.0.21 + drop legacy ruzstd Changelog.md ([#155](https://github.com/structured-world/structured-zstd/pull/155))

### Performance

- *(decode)* pack LL/ML metadata + hot-path micro-opts ([#197](https://github.com/structured-world/structured-zstd/pull/197))
- *(decode)* fused sequence executor + SIMD/FSE hot-path cleanup + DoS-safe rollback ([#194](https://github.com/structured-world/structured-zstd/pull/194))
- *(encoder)* align Fast strategy window_log with donor (17 → 19) ([#187](https://github.com/structured-world/structured-zstd/pull/187))
- *(encoder)* inline hash-chain walk into hash_chain_candidate (lazy L1) ([#185](https://github.com/structured-world/structured-zstd/pull/185))
- *(encoder)* G3 — whole-block bail-out before partition split ([#181](https://github.com/structured-world/structured-zstd/pull/181))
- *(encoder)* donor-parity greedy parse at L4 — ratio + speed win ([#179](https://github.com/structured-world/structured-zstd/pull/179))
- *(huff0)* cache encoded weight-description bytes on `HuffmanTable` and reuse in emit path ([#170](https://github.com/structured-world/structured-zstd/pull/170))
- *(huff0)* #167 cheap entropy proxy for table_log selection — no FSE-encode per candidate ([#168](https://github.com/structured-world/structured-zstd/pull/168))
- *(fse)* donor FSE_buildCTable_wksp parity — drop per-symbol Vec<State> ([#166](https://github.com/structured-world/structured-zstd/pull/166))
- *(fse)* replace next_state linear search with donor-parity flat tables + tune CI bench budgets ([#165](https://github.com/structured-world/structured-zstd/pull/165))

## [0.0.21](https://github.com/structured-world/structured-zstd/compare/v0.0.20...v0.0.21) - 2026-05-17

### Added

- *(encoding)* #23 add donor _fromBorders pre-split heuristic + broaden level coverage ([#140](https://github.com/structured-world/structured-zstd/pull/140))

### Fixed

- *(release)* untrack fuzz crash artifacts already gitignored ([#148](https://github.com/structured-world/structured-zstd/pull/148))
- *(bench)* restore sample_size=10, raise measurement_time to clear criterion floor ([#144](https://github.com/structured-world/structured-zstd/pull/144))

### Performance

- *(7pre)* enablement — donor outer/inner dfast Level 3 + cross-level FSE cache ([#146](https://github.com/structured-world/structured-zstd/pull/146))
- *(encoding)* #18 Phase 5 — LDM producer (gear hash + bucket table + search/emit) ([#139](https://github.com/structured-world/structured-zstd/pull/139))
- *(encoding)* #124 Phase 4 — HC speculative tail check + MatcherStorage enum dispatch ([#125](https://github.com/structured-world/structured-zstd/pull/125))
- *(encoding)* #111 Phase 3 — const-generic Strategy dispatch ([#123](https://github.com/structured-world/structured-zstd/pull/123))
- *(encoding)* #111 Phase 2 — saturating_* cleanup on hot path ([#121](https://github.com/structured-world/structured-zstd/pull/121))

### Refactored

- *(encoding)* #111 Phase 1e — migrate methods off HcMatchGenerator (Stages A–D) ([#119](https://github.com/structured-world/structured-zstd/pull/119))
- *(encoding)* #111 Phase 1d — split HcMatchGenerator into MatchTable / HcMatcher / BtMatcher ([#118](https://github.com/structured-world/structured-zstd/pull/118))
- *(encoding)* #111 Phase 1c — extract Simple matcher into simple/ ([#117](https://github.com/structured-world/structured-zstd/pull/117))
- *(encoding)* #111 Phase 1b — extract shared match-finder helpers ([#116](https://github.com/structured-world/structured-zstd/pull/116))
- *(encoding)* #111 Phase 1 — structural split of match_generator monolith ([#113](https://github.com/structured-world/structured-zstd/pull/113))

### Testing

- *(fuzz)* make test_all_artifacts skip when corpus dir is absent ([#151](https://github.com/structured-world/structured-zstd/pull/151))

### Bench

- add real peak-alloc metric for full encode/decode/memory baseline ([#143](https://github.com/structured-world/structured-zstd/pull/143))

## [0.0.20](https://github.com/structured-world/structured-zstd/compare/v0.0.19...v0.0.20) - 2026-05-12

### Performance

- *(decoding)* port upstream extend_and_fill / extend_from_reader for RLE+Raw blocks ([#114](https://github.com/structured-world/structured-zstd/pull/114))
- *(level22)* complete donor parity path ([#110](https://github.com/structured-world/structured-zstd/pull/110))

## [0.0.19](https://github.com/structured-world/structured-zstd/compare/v0.0.18...v0.0.19) - 2026-04-12

### Performance

- *(bench)* add wildcopy candidate research bench and dashboard range filter ([#107](https://github.com/structured-world/structured-zstd/pull/107))

## [0.0.18](https://github.com/structured-world/structured-zstd/compare/v0.0.17...v0.0.18) - 2026-04-12

### Performance

- *(decoding)* add shared dictionary handle ([#105](https://github.com/structured-world/structured-zstd/pull/105))

## [0.0.17](https://github.com/structured-world/structured-zstd/compare/v0.0.16...v0.0.17) - 2026-04-11

### Performance

- *(encoding)* complete ARM histogram path for #71 ([#104](https://github.com/structured-world/structured-zstd/pull/104))
- *(encoding)* CRC-gated hash mix for ARM and x86_64 ([#102](https://github.com/structured-world/structured-zstd/pull/102))

## [0.0.16](https://github.com/structured-world/structured-zstd/compare/v0.0.15...v0.0.16) - 2026-04-11

### Performance

- *(encoding)* early incompressible fast-path + benchmark parity ([#99](https://github.com/structured-world/structured-zstd/pull/99))

## [0.0.15](https://github.com/structured-world/structured-zstd/compare/v0.0.14...v0.0.15) - 2026-04-09

### Performance

- *(encoding)* SIMD-dispatch common_prefix_len ([#96](https://github.com/structured-world/structured-zstd/pull/96))

## [0.0.14](https://github.com/structured-world/structured-zstd/compare/v0.0.13...v0.0.14) - 2026-04-09

### Performance

- *(encoding)* enable one-shot size hint across levels safely ([#94](https://github.com/structured-world/structured-zstd/pull/94))

## [0.0.13](https://github.com/structured-world/structured-zstd/compare/v0.0.12...v0.0.13) - 2026-04-09

### Performance

- *(decoding)* SIMD HUF kernels with runtime dispatch ([#92](https://github.com/structured-world/structured-zstd/pull/92))

## [0.0.12](https://github.com/structured-world/structured-zstd/compare/v0.0.11...v0.0.12) - 2026-04-09

### Performance

- *(decoding)* branchless offset history, prefetch pipeline, and BMI2 triple extract ([#90](https://github.com/structured-world/structured-zstd/pull/90))

## [0.0.11](https://github.com/structured-world/structured-zstd/compare/v0.0.10...v0.0.11) - 2026-04-09

### Performance

- *(decoding)* add runtime-dispatched simd wildcopy ([#85](https://github.com/structured-world/structured-zstd/pull/85))

## [0.0.10](https://github.com/structured-world/structured-zstd/compare/v0.0.9...v0.0.10) - 2026-04-08

### Performance

- *(encoding)* add row-based match finder backend ([#84](https://github.com/structured-world/structured-zstd/pull/84))
- *(encoding)* rebase hc positions past u32 boundary ([#82](https://github.com/structured-world/structured-zstd/pull/82))

## [0.0.9](https://github.com/structured-world/structured-zstd/compare/v0.0.8...v0.0.9) - 2026-04-08

### Performance

- *(encoding)* reuse streaming encoded scratch buffer ([#80](https://github.com/structured-world/structured-zstd/pull/80))

## [0.0.8](https://github.com/structured-world/structured-zstd/compare/v0.0.7...v0.0.8) - 2026-04-07

### Fixed

- *(ci)* publish benchmark delta reports ([#75](https://github.com/structured-world/structured-zstd/pull/75))

### Performance

- *(bench)* multi-arch relative Rust-vs-FFI dashboard ([#78](https://github.com/structured-world/structured-zstd/pull/78))
- *(fse)* pack decoder entries and align decode tables ([#76](https://github.com/structured-world/structured-zstd/pull/76))
- *(bench)* add fastcover vs ffi dict-training delta ([#73](https://github.com/structured-world/structured-zstd/pull/73))

## [0.0.7](https://github.com/structured-world/structured-zstd/compare/v0.0.6...v0.0.7) - 2026-04-05

### Added

- *(encoding)* numeric compression levels (1-22) API ([#63](https://github.com/structured-world/structured-zstd/pull/63))

### Performance

- *(bench)* add rust/ffi delta benchmark artifacts ([#65](https://github.com/structured-world/structured-zstd/pull/65))

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
