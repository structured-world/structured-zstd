# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.45](https://github.com/structured-world/structured-zstd/compare/v0.0.44...v0.0.45) - 2026-06-25

### Performance

- *(opt)* close the btopt dict-band gap via upstream getAllMatches shape ([#448](https://github.com/structured-world/structured-zstd/pull/448))

### Refactored

- *(encoding)* split match_generator monolith + separate tests from production ([#445](https://github.com/structured-world/structured-zstd/pull/445))

## [0.0.44](https://github.com/structured-world/structured-zstd/compare/v0.0.43...v0.0.44) - 2026-06-23

### Performance

- *(kernel)* disable AVX-512 VBMI2 tier by default; defer dict-frame table clear ([#443](https://github.com/structured-world/structured-zstd/pull/443))

## [0.0.43](https://github.com/structured-world/structured-zstd/compare/v0.0.42...v0.0.43) - 2026-06-23

### Performance

- *(encode)* close the L6-L9 lazy dictionary compress-speed gap ([#442](https://github.com/structured-world/structured-zstd/pull/442))
- *(encode)* reuse the LDM producer table across dict frames ([#440](https://github.com/structured-world/structured-zstd/pull/440))
- *(decode)* branchy fused decodeSequence on the AVX2 short arm ([#439](https://github.com/structured-world/structured-zstd/pull/439))
- *(decode)* HUF table-fill by code-length group + scalarize 4-stream burst ([#437](https://github.com/structured-world/structured-zstd/pull/437))

## [0.0.41](https://github.com/structured-world/structured-zstd/compare/v0.0.40...v0.0.41) - 2026-06-17

### Performance

- *(decode)* decode StreamingDecoder read_to_end in place ([#429](https://github.com/structured-world/structured-zstd/pull/429))
- *(hc)* lazy-skip + separate dictMatchState beat C on small-window dict compress ([#427](https://github.com/structured-world/structured-zstd/pull/427))
- *(encode)* close small-window dict compress gap (hash-chain matchfinder + borrowed dict kernel) ([#426](https://github.com/structured-world/structured-zstd/pull/426))
- *(huff0)* skip FSE weight description for streams it cannot encode ([#424](https://github.com/structured-world/structured-zstd/pull/424))

### Refactored

- purge reference-impl terminology + isolate C bindings in ffi-bench ([#430](https://github.com/structured-world/structured-zstd/pull/430))

## [0.0.40](https://github.com/structured-world/structured-zstd/compare/v0.0.39...v0.0.40) - 2026-06-15

### Performance

- *(huff0)* hoist bit-stream state into locals in the encode loop ([#423](https://github.com/structured-world/structured-zstd/pull/423))
- borrowed in-place over-window scan for dfast + row + btlazy2 ([#422](https://github.com/structured-world/structured-zstd/pull/422))
- *(encode)* cap HC/BT history mirror near the live window ([#421](https://github.com/structured-world/structured-zstd/pull/421))
- *(encode)* cap dfast history buffer near the live window ([#420](https://github.com/structured-world/structured-zstd/pull/420))
- *(encode)* cap row history buffer near the live window ([#418](https://github.com/structured-world/structured-zstd/pull/418))

## [0.0.39](https://github.com/structured-world/structured-zstd/compare/v0.0.38...v0.0.39) - 2026-06-14

### Performance

- *(encode)* cap row match-finder table to upstream hashLog ([#417](https://github.com/structured-world/structured-zstd/pull/417))
- *(encode)* single price arena + interleaved cache in the optimal parser ([#415](https://github.com/structured-world/structured-zstd/pull/415))

## [0.0.38](https://github.com/structured-world/structured-zstd/compare/v0.0.37...v0.0.38) - 2026-06-14

### Added

- *(c-api)* complete the stable ZSTDLIB_API surface + btopt/btultra perf ([#413](https://github.com/structured-world/structured-zstd/pull/413))

## [0.0.37](https://github.com/structured-world/structured-zstd/compare/v0.0.36...v0.0.37) - 2026-06-13

### Added

- *(c-api)* dictionary attach surface + estimates + fastCover; wasm prepared dictionary ([#409](https://github.com/structured-world/structured-zstd/pull/409))

## [0.0.36](https://github.com/structured-world/structured-zstd/compare/v0.0.35...v0.0.36) - 2026-06-12

### Performance

- dict decode peak + direct path + repcode gate + entropy table build ([#403](https://github.com/structured-world/structured-zstd/pull/403))

## [0.0.35](https://github.com/structured-world/structured-zstd/compare/v0.0.34...v0.0.35) - 2026-06-11

### Added

- *(c-api)* advanced parameter API + streaming surface (compressStream2 / decompressStream) ([#400](https://github.com/structured-world/structured-zstd/pull/400))

### Performance

- *(encode)* size the output reservation from the observed ratio ([#398](https://github.com/structured-world/structured-zstd/pull/398))
- *(encode)* allocation-free copy-mode dictionary restore ([#397](https://github.com/structured-world/structured-zstd/pull/397))

## [0.0.34](https://github.com/structured-world/structured-zstd/compare/v0.0.33...v0.0.34) - 2026-06-10

### Added

- dictionary CDict-equivalent (encoder + C ABI + wasm + CLI); L22 dict memory 8.2x->1.13x ([#387](https://github.com/structured-world/structured-zstd/pull/387))
- *(c-api)* C ABI core (cdylib + vendored headers + simple/context/error/frame/dict wrappers) ([#386](https://github.com/structured-world/structured-zstd/pull/386))
- content-checksum modes + npm bindings, RingBuffer wrapped inline match-copy ([#385](https://github.com/structured-world/structured-zstd/pull/385))

### Performance

- *(encode)* zero per-frame allocations on the reused-compressor path ([#395](https://github.com/structured-world/structured-zstd/pull/395))
- *(decode)* hash each block while cache-hot on the direct path ([#394](https://github.com/structured-world/structured-zstd/pull/394))
- *(encode)* [**breaking**] cut dict-compress per-frame overhead; shrink wasm payload 18% ([#393](https://github.com/structured-world/structured-zstd/pull/393))
- *(encode)* monolithize dfast per-match helpers into the kernel ([#391](https://github.com/structured-world/structured-zstd/pull/391))
- *(encode)* 4-byte gate before HC chain common_prefix_len ([#392](https://github.com/structured-world/structured-zstd/pull/392))
- *(encode)* reshape dfast match-find toward donor structure ([#390](https://github.com/structured-world/structured-zstd/pull/390))
- *(encode)* close the level-4 dfast speed outlier (donor greedy double-fast) ([#389](https://github.com/structured-world/structured-zstd/pull/389))
- *(decode)* cut per-frame/block overhead (RingBuffer inline exec, FSE arrays, dict copy-on-write) ([#381](https://github.com/structured-world/structured-zstd/pull/381))
- *(decode)* cut HUF/FSE entropy-build overhead on the decode hot path ([#377](https://github.com/structured-world/structured-zstd/pull/377))

## [0.0.33](https://github.com/structured-world/structured-zstd/compare/v0.0.32...v0.0.33) - 2026-06-08

### Performance

- dict-attach across match finders + decode peak-alloc ([#359](https://github.com/structured-world/structured-zstd/pull/359))

## [0.0.31](https://github.com/structured-world/structured-zstd/compare/v0.0.30...v0.0.31) - 2026-06-07

### Added

- *(wasm)* streaming compress ZstdCompressStream + align decoder window cap to zstd default ([#367](https://github.com/structured-world/structured-zstd/pull/367))
- *(wasm)* simd128 kernel tier + npm package; LDM bench matrix variants ([#364](https://github.com/structured-world/structured-zstd/pull/364))
- *(encode)* configurable compression parameters API ([#360](https://github.com/structured-world/structured-zstd/pull/360))

### Harden

- *(decode)* uniform FrameContentSizeMismatch on Compressed overshoot ([#363](https://github.com/structured-world/structured-zstd/pull/363))

## [0.0.30](https://github.com/structured-world/structured-zstd/compare/v0.0.29...v0.0.30) - 2026-06-07

### Added

- *(decoding)* block-subset partial decode + per-block decompressed byte ranges ([#357](https://github.com/structured-world/structured-zstd/pull/357))

### Performance

- *(encode)* inline SIMD literal copy for medium runs ([#355](https://github.com/structured-world/structured-zstd/pull/355))

## [0.0.29](https://github.com/structured-world/structured-zstd/compare/v0.0.28...v0.0.29) - 2026-06-06

### Added

- *(bench)* report active CPU kernel tier + fix decode_loop corpus masking ([#351](https://github.com/structured-world/structured-zstd/pull/351))

### Documentation

- *(readme)* fix btultra/btultra2 level boundary in strategy table ([#334](https://github.com/structured-world/structured-zstd/pull/334))

### Fixed

- *(decode)* bound per-block output (decompression-bomb OOM) + L19 decode gap ([#350](https://github.com/structured-world/structured-zstd/pull/350))

### Performance

- *(codec)* per-strategy block-split levels + per-tier exec-macro seq monolith ([#354](https://github.com/structured-world/structured-zstd/pull/354))
- *(codec)* warm dictionary reuse (decode + encode) + relocate decompression-bomb guard ([#352](https://github.com/structured-world/structured-zstd/pull/352))
- *(encode)* store row match positions as u32 to cut peak memory ([#346](https://github.com/structured-world/structured-zstd/pull/346))
- *(encode)* right-size dfast/row/hash-chain tables for small inputs ([#345](https://github.com/structured-world/structured-zstd/pull/345))
- *(encode)* price the custom FSE table without building it ([#344](https://github.com/structured-world/structured-zstd/pull/344))
- *(encode)* cut small-input fixed cost on the fast levels ([#342](https://github.com/structured-world/structured-zstd/pull/342))
- *(encode)* hoist hash/chain fill rebase guard out of insert loop ([#340](https://github.com/structured-world/structured-zstd/pull/340))
- *(encode)* rebase matcher floor on reset instead of zeroing tables ([#338](https://github.com/structured-world/structured-zstd/pull/338))
- *(encode)* donor-correct block-split + block-precise decode errors ([#336](https://github.com/structured-world/structured-zstd/pull/336))
- *(encode)* decouple and monomorphize the row matcher ([#335](https://github.com/structured-world/structured-zstd/pull/335))
- *(encode)* close level-5 greedy speed gap (row match-span cap) ([#331](https://github.com/structured-world/structured-zstd/pull/331))
- *(encode)* donor row-0 params for Fast negative levels ([#330](https://github.com/structured-world/structured-zstd/pull/330))
- *(encode)* size-gated dictionary match-finding for the Fast strategy ([#328](https://github.com/structured-world/structured-zstd/pull/328))
- *(dict)* greedy set-cover segment selection in fastcover trainer ([#324](https://github.com/structured-world/structured-zstd/pull/324))

## [0.0.28](https://github.com/structured-world/structured-zstd/compare/v0.0.27...v0.0.28) - 2026-06-02

### Added

- *(decode)* kernel_* cargo features for per-tier CPU kernel selection ([#307](https://github.com/structured-world/structured-zstd/pull/307))

### Performance

- *(encode)* borrow one-shot input as Fast match window ([#318](https://github.com/structured-world/structured-zstd/pull/318))
- *(encode)* raw-pointer reads in Fast kernel backward extension ([#321](https://github.com/structured-world/structured-zstd/pull/321))
- *(decode)* route RLE-mode sequence tables through the fused path ([#320](https://github.com/structured-world/structured-zstd/pull/320))
- *(encode)* drop redundant per-match offset coding in Fast matcher ([#319](https://github.com/structured-world/structured-zstd/pull/319))
- *(encode)* dedup window<->history live-region storage ([#317](https://github.com/structured-world/structured-zstd/pull/317))
- *(encode)* map level 19 to btultra2 + align bench labels to clevels.h ([#315](https://github.com/structured-world/structured-zstd/pull/315))
- *(encode)* skip decoder-table build when loading a dict for encoding ([#314](https://github.com/structured-world/structured-zstd/pull/314))
- *(decode)* route bulk non-overlapping copies through memcpy (ERMS) ([#309](https://github.com/structured-world/structured-zstd/pull/309))
- *(encode)* vectorize the Row match-finder tag scan ([#305](https://github.com/structured-world/structured-zstd/pull/305))
- *(encode)* lower row-matcher min match to 5 (donor parity) ([#310](https://github.com/structured-world/structured-zstd/pull/310))
- *(encode)* align levels 13-15 to reference search budget ([#302](https://github.com/structured-world/structured-zstd/pull/302))
- *(decode)* unify SIMD-copy capability detection under the CpuKernel tag ([#304](https://github.com/structured-world/structured-zstd/pull/304))

### Bench

- *(dict)* report Rust dict-compressed size + emit compress-dict ratio ([#301](https://github.com/structured-world/structured-zstd/pull/301))

## [0.0.27](https://github.com/structured-world/structured-zstd/compare/v0.0.26...v0.0.27) - 2026-05-31

### Performance

- *(decode)* expand overlapping matches by doubling, not offset chunks ([#300](https://github.com/structured-world/structured-zstd/pull/300))
- *(decode)* inline donor match-copy on all targets via portable wildcopy ([#299](https://github.com/structured-world/structured-zstd/pull/299))
- *(decode)* unroll HUF 4-stream burst inner loop via const-generic ([#296](https://github.com/structured-world/structured-zstd/pull/296))
- *(decode)* skip zero-init of literals target — HUF overwrites everything ([#295](https://github.com/structured-world/structured-zstd/pull/295))
- *(fse)* kill iterator overhead in build_decoding_table, write decode via set_len ([#293](https://github.com/structured-world/structured-zstd/pull/293))
- *(decode)* straight-loop short path + donor-gated lookahead ring + SeqSymbol repack ([#289](https://github.com/structured-world/structured-zstd/pull/289))
- *(decode)* skip post-decode XXH64 when content_checksum_flag is 0 ([#287](https://github.com/structured-world/structured-zstd/pull/287))
- *(decode)* per-tier x86 kernel split + AVX2 32-byte match-copy (#279 Phase 3+4) ([#285](https://github.com/structured-world/structured-zstd/pull/285))
- *(decode)* lazy ring-buffer allocation for direct-eligible frames ([#282](https://github.com/structured-world/structured-zstd/pull/282))
- *(decode)* drop dead seq.of from pipelined ring slots ([#283](https://github.com/structured-world/structured-zstd/pull/283))

### Testing

- *(bench)* add encode_loop_z000033 example for clean encoder profiles ([#297](https://github.com/structured-world/structured-zstd/pull/297))
- *(decode)* decode_loop binary — add --mode ffi and --corpus path ([#288](https://github.com/structured-world/structured-zstd/pull/288))

## [0.0.26](https://github.com/structured-world/structured-zstd/compare/v0.0.25...v0.0.26) - 2026-05-27

### Added

- *(encoding+decoding)* FrameEmitInfo + opt-in per-block XXH64 sidecar ([#272](https://github.com/structured-world/structured-zstd/pull/272))
- *(decoding)* skippable-payload visitor callback on FrameDecoder ([#271](https://github.com/structured-world/structured-zstd/pull/271))

### Documentation

- *(#176)* Skippable Frame Magic Allocations registry (#270)

### Performance

- *(decode)* drop inline_never on repcode resolver, keep cold attr ([#281](https://github.com/structured-world/structured-zstd/pull/281))
- *(encoding)* HUF_flags_preferRepeat for fast strategies + small literals (#23 G6) ([#278](https://github.com/structured-world/structured-zstd/pull/278))
- *(decoding)* mirror donor ddictIsCold signal for pipelined dispatch ([#274](https://github.com/structured-world/structured-zstd/pull/274))
- *(fse)* rewrite build_decoding_table per donor ZSTD_buildFSETable_body shape ([#276](https://github.com/structured-world/structured-zstd/pull/276))
- *(decode)* route short-block fallback through inline executor (z000033 −25%) ([#269](https://github.com/structured-world/structured-zstd/pull/269))
- *(decode)* cache predefined FSE tables (small-4k-log-lines −69%) ([#268](https://github.com/structured-world/structured-zstd/pull/268))
- *(decode)* inline sequence executor for direct path + auto-route decode_all (z000033 −24%, high-entropy-1m parity) ([#263](https://github.com/structured-world/structured-zstd/pull/263))
- *(bench)* pre-touch decompress output Vec to kill page-fault artifact ([#260](https://github.com/structured-world/structured-zstd/pull/260))
- *(decode)* bump WILDCOPY_OVERLENGTH 16 → 32 for AVX2 chunked kernel reach ([#261](https://github.com/structured-world/structured-zstd/pull/261))
- *(decode)* RingBuffer % cap → branchless wrap helper (kills divl on i686, divq on x86_64) ([#255](https://github.com/structured-world/structured-zstd/pull/255))
- *(decode)* #247 Part 2 — kill divb in repeat_short_offset + force-inline UserSliceBackend::extend ([#254](https://github.com/structured-world/structured-zstd/pull/254))
- *(decode)* #247 Part 1 — expand FSE Entry to ZSTD_seqSymbol shape ([#252](https://github.com/structured-world/structured-zstd/pull/252))

### Testing

- *(bench)* unblock dict-driven bench matrix + add pure_rust_with_dict compress arm ([#277](https://github.com/structured-world/structured-zstd/pull/277))

### Harden

- *(decode)* fallible BufferBackend writes for RLE/Raw direct path ([#251](https://github.com/structured-world/structured-zstd/pull/251))

## [0.0.25](https://github.com/structured-world/structured-zstd/compare/v0.0.24...v0.0.25) - 2026-05-24

### Added

- *(skippable)* typed SkippableFrame API behind lsm feature ([#248](https://github.com/structured-world/structured-zstd/pull/248))
- *(decode)* expect_dict_id + expect_window_descriptor setters on FrameDecoder ([#249](https://github.com/structured-world/structured-zstd/pull/249))

### Performance

- *(decode)* direct-write decode_to_slice path ([#244](https://github.com/structured-world/structured-zstd/pull/244)) ([#245](https://github.com/structured-world/structured-zstd/pull/245))

## [0.0.24](https://github.com/structured-world/structured-zstd/compare/v0.0.23...v0.0.24) - 2026-05-24

### Performance

- *(decode)* pack HUF decode table as u16 (donor HUF_DEltX1 layout) ([#243](https://github.com/structured-world/structured-zstd/pull/243))
- *(decode)* collapse HUF 4-stream burst-gate to single-cursor olimit ([#238](https://github.com/structured-world/structured-zstd/pull/238))
- *(encoder)* align lazy-band target_len with donor clevels.h table[0] ([#239](https://github.com/structured-world/structured-zstd/pull/239))
- *(fast)* donor-parity hot-path cleanups in Fast kernel + inline short-literal append (#220 follow-up) ([#231](https://github.com/structured-world/structured-zstd/pull/231))

### Testing

- *(encoding)* donor-parity comparator for block splitter port ([#240](https://github.com/structured-world/structured-zstd/pull/240))
- *(bench)* add decompress-dict group to compare_ffi ([#236](https://github.com/structured-world/structured-zstd/pull/236))

## [0.0.23](https://github.com/structured-world/structured-zstd/compare/v0.0.22...v0.0.23) - 2026-05-23

### Added

- *(frame)* magicless frame format support ([#26](https://github.com/structured-world/structured-zstd/pull/26)) ([#222](https://github.com/structured-world/structured-zstd/pull/222))

### Performance

- *(row)* drop eager block-boundary inserts ([#180](https://github.com/structured-world/structured-zstd/pull/180)) ([#232](https://github.com/structured-world/structured-zstd/pull/232))
- *(decode)* 8-slot software prefetch pipeline for sequence execution ([#208](https://github.com/structured-world/structured-zstd/pull/208)) ([#227](https://github.com/structured-world/structured-zstd/pull/227))
- *(encoder)* port donor ZSTD_compressBlock_fast — 4-cursor + per-level cParams + cmov + window-correctness (#198 phase 3) ([#229](https://github.com/structured-world/structured-zstd/pull/229))
- *(decoding)* integrate AVX2 unroll-2 wildcopy candidate ([#108](https://github.com/structured-world/structured-zstd/pull/108)) ([#223](https://github.com/structured-world/structured-zstd/pull/223))
- *(encoder)* wire donor-shape Fast kernel into MatchGeneratorDriver (#198 phase 1b) ([#217](https://github.com/structured-world/structured-zstd/pull/217))
- *(encoder)* donor-shape Fast kernel modules (#198 phase 1a) ([#215](https://github.com/structured-world/structured-zstd/pull/215))
- *(fse)* elide bounds check on init_state + update_state decode reads ([#214](https://github.com/structured-world/structured-zstd/pull/214))
- *(decode)* SIMD-16 fast path for short offsets {1, 2, 4} ([#213](https://github.com/structured-world/structured-zstd/pull/213))
- *(decode)* const-generic HUF kernel monomorphisation for SIMD-fallback ([#212](https://github.com/structured-world/structured-zstd/pull/212))
- *(decode)* port donor HUF 4-stream burst with sentinel-bit ctz ([#201](https://github.com/structured-world/structured-zstd/pull/201))

### Testing

- *(hc)* cross-slice boundary position seeding regression test ([#235](https://github.com/structured-world/structured-zstd/pull/235))

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
