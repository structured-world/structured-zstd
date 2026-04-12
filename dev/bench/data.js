window.BENCHMARK_DATA = {
  "lastUpdate": 1776023005864,
  "repoUrl": "https://github.com/structured-world/structured-zstd",
  "entries": {
    "structured-zstd vs C FFI": [
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "78e25c1128d80a6978a5dca8ac5c71150b42bccc",
          "message": "feat: managed fork setup — README, FUNDING, crate rename to structured-zstd (#2)\n\n* docs(readme): managed fork header, donate QR, FUNDING.yml\n\nCloses #1\n\n* chore(crate): rename to structured-zstd v0.0.1, update metadata\n\n- Package name: ruzstd → structured-zstd\n- Repository/homepage: point to structured-world fork\n- Add SW Foundation as co-author\n- Version: 0.0.1 (initial fork release)\n\nCloses #1\n\n* ci: add CI matrix, release-plz, codecov, nextest, coderabbit\n\n- CI: lint, test (3 OS × stable + i686 cross + MSRV 1.87), codecov, bench\n- release-plz + cargo publish via OIDC\n- Dependabot auto-merge for minor/patch\n- nextest with ci profile\n- License: MIT → Apache-2.0\n\n* test(bench): add FFI comparison benchmark, fix crate rename references\n\n- compare_ffi bench: pure Rust vs C FFI zstd decompression\n- Baseline: pure Rust 6.02ms vs C FFI 2.80ms (2.15x slower)\n- Fix cli/Cargo.toml dependency: ruzstd → structured-zstd\n- Fix bench/cli imports for renamed crate\n\n* docs(readme): fix import paths to structured_zstd, absolute QR URL, fix description\n\n* ci(bench): github-action-benchmark with 5 decompress/compress variations\n\n- decompress: pure Rust vs C FFI\n- compress: pure Rust fastest vs C FFI level 1 vs C FFI level 3\n- Regression alerts: 15% warn, 25% fail\n- Results stored in gh-pages via SW Release Bot\n\n* chore(license): fill Apache-2.0 copyright placeholder\n\n* fix(ci): replace --all-features with explicit features, exclude rustc-dep-of-std\n\nrustc-dep-of-std pulls compiler_builtins which requires nightly.\nUse --features hash,std,dict_builder for all CI jobs.\n\n* fix(docs): update include_str path after Readme.md → README.md rename\n\n* fix(dict): isize → i64 in Karp-Rabin hash for 32-bit targets\n\n* test(integrity): add 5000-iteration roundtrip integrity tests\n\n- roundtrip_random_data: 1000 iterations, random data 0-64KB\n- roundtrip_compressible_data: 1000 iterations, repeating patterns\n- roundtrip_streaming_api: 1000 iterations via FrameCompressor\n- cross_validation: 1000 iterations rust→ffi + 1000 ffi→rust\n- edge cases: empty, single byte, all zeros, all 0xFF, ascending, 1MB RLE\n- nextest ci profile with retries and JUnit XML\n\n* fix(fork): retain upstream MIT notice, fix bench black_box, pipefail\n\n- NOTICE: retain upstream MIT license attribution (Moritz Borcherding)\n- Benchmarks: wrap compress outputs in black_box to prevent elision\n- Benchmark script: add pipefail, remove stderr suppression, fail on no results\n- cli/Cargo.toml: fix readme path Readme.md → README.md\n\n* fix(lint): suppress dead_code warnings on test-only roundtrip helpers\n\n* fix(docs): replace ruzstd:: with structured_zstd:: in all doc examples\n\n- 8 doc-test files updated: encoding, decoding, dictionary, lib\n- README decompression example: add no_run + initialize compressed_data\n- gh-pages branch created for benchmark-action data storage\n\n* fix(dict): correct Karp-Rabin h precompute loop and signed modulo\n\n* fix(dict): guard empty pattern in estimate_frequency, pin QR URL to main\n\n* fix(ci): add -p structured-zstd for workspace feature resolution, update cli description\n\n* fix(ci): set working-directory to ruzstd/ for nextest (corpus file paths)\n\n* refactor(workspace): rename ruzstd/ → zstd/, purge all ruzstd references\n\n- Directory: ruzstd/ → zstd/\n- Workspace members: [\"zstd\", \"cli\"]\n- CLI: ruzstd-cli → structured-zstd-cli, direct structured_zstd imports\n- Fuzz targets: structured_zstd imports, renamed helper functions\n- Internal test helpers: decode_ruzstd → decode_szstd, encode_ruzstd → encode_szstd\n- CI: working-directory zstd/, lcov path updated\n- Only \"ruzstd\" remaining: upstream attribution in README/description\n\n* fix(dict): fix occurances → occurrences typo in dictionary module\n\n* fix(cli): update homepage, repository, license to match fork\n\n* fix(bench): symmetric per-iteration allocation in decompress benchmark\n\n* fix(test): mark corpus files as binary to prevent CRLF conversion on Windows\n\n* fix(test): use recursive glob in .gitattributes for dict_tests subdirs",
          "timestamp": "2026-03-25T06:28:45+02:00",
          "tree_id": "0970fd9f99603dbc22c9c6c7209f5e50b5ccfa5a",
          "url": "https://github.com/structured-world/structured-zstd/commit/78e25c1128d80a6978a5dca8ac5c71150b42bccc"
        },
        "date": 1774413074564,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9.102,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.917,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.363,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.485,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.085,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "distinct": true,
          "id": "43e2112072edb05483e7254554a16f6cc0933993",
          "message": "fix(ci): fix TOML syntax error in release-plz config",
          "timestamp": "2026-03-25T12:07:35+02:00",
          "tree_id": "78ed0ad83abf80d54d6f9b1d4979d182accdf7a9",
          "url": "https://github.com/structured-world/structured-zstd/commit/43e2112072edb05483e7254554a16f6cc0933993"
        },
        "date": 1774433359995,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9.105,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.922,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 15.845,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 2.78,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 4.977,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "618187d27068d6dc7bae732253aec37d0e9aa31e",
          "message": "chore(structured-zstd): release v0.0.2 (#3)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-03-25T18:40:51+02:00",
          "tree_id": "5ad4381857ae03dece35edab1166225e44aba7ac",
          "url": "https://github.com/structured-world/structured-zstd/commit/618187d27068d6dc7bae732253aec37d0e9aa31e"
        },
        "date": 1774456955704,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9.003,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.916,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.242,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.459,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 4.931,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "a4d854ca52d94760a41151c312b2fa699d03133d",
          "message": "fix: update README — remove upstream contribution claims (#29)\n\n- Remove inaccurate \"contributing patches upstream\" statement\n- Replace \"Upstream sync\" with accurate fork relationship description\n- Add compression levels to fork goals list\n\nCloses #4",
          "timestamp": "2026-03-25T19:08:30+02:00",
          "tree_id": "3d49226489071c9ac28a747624603a84f4f7b7bd",
          "url": "https://github.com/structured-world/structured-zstd/commit/a4d854ca52d94760a41151c312b2fa699d03133d"
        },
        "date": 1774458602950,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9.044,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.924,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 15.651,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 2.766,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.032,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "distinct": true,
          "id": "d42d53eb3f91899b8536822d44dfa8fa785440b5",
          "message": "fix(ci): disable publish in release-plz — handled by release.yml\n\nrelease-plz release command was trying to cargo publish without\nCARGO_REGISTRY_TOKEN. Publishing is handled separately by release.yml\nvia trusted publishing on GitHub Release creation.",
          "timestamp": "2026-03-25T19:39:36+02:00",
          "tree_id": "a25ff362cf0e88d7a2067c4c952d87a6aea70917",
          "url": "https://github.com/structured-world/structured-zstd/commit/d42d53eb3f91899b8536822d44dfa8fa785440b5"
        },
        "date": 1774460477195,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 8.486,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.857,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 17.548,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.028,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 4.46,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "distinct": true,
          "id": "67681d7c6e1a515cbaee5b9a6e00f2ecf7f436e1",
          "message": "fix: use local Readme.md for crate readme\n\ncargo publish cannot include files outside the crate directory.\nChanged from ../README.md to local zstd/Readme.md.",
          "timestamp": "2026-03-25T19:41:14+02:00",
          "tree_id": "68f979ebf16df17bb906a3270858bd18b347d674",
          "url": "https://github.com/structured-world/structured-zstd/commit/67681d7c6e1a515cbaee5b9a6e00f2ecf7f436e1"
        },
        "date": 1774460579432,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9.101,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.9,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.204,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.488,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.012,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "90378eba02e39add0c20161b0a09b7d9b163c19e",
          "message": "chore: bump MSRV to 1.92, ignore dtolnay/rust-toolchain in dependabot (#32)\n\n- Update rust-version in zstd/Cargo.toml and cli/Cargo.toml to 1.92\n- Update MSRV CI job toolchain from 1.87 to 1.92\n- Exclude dtolnay/rust-toolchain from dependabot updates\n\nCloses #31",
          "timestamp": "2026-03-25T20:14:26+02:00",
          "tree_id": "1c64a43ae32f06767608f0c92298bfd54ec22455",
          "url": "https://github.com/structured-world/structured-zstd/commit/90378eba02e39add0c20161b0a09b7d9b163c19e"
        },
        "date": 1774462611744,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 8.491,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.871,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 15.82,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 2.597,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 4.258,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "1c9c616fea994ca4a257d48cfb28c798a5284b7d",
          "message": "feat: large literals block support (>262KB) (#30)\n\n* feat: replace unimplemented! panic with proper error handling for large literals\n\n- Replace `unimplemented!(\"too many literals\")` with `unreachable!` and\n  descriptive message explaining the MAX_BLOCK_SIZE invariant\n- Add debug_assert validating literals never exceed MAX_BLOCK_SIZE (128KB)\n- Add RFC 8878 §3.1.1.3.1.1 size format documentation comments\n- Add roundtrip tests exercising all 4 size format boundaries (0b00–0b11)\n- Add cross-validation tests (Rust ↔ C FFI) for large blocks up to 128KB\n\nCloses #15\n\n* docs: clarify reachable size formats in encoder and test docs\n\n- Remove redundant debug_assert (unreachable! already guards the invariant)\n- Clarify RFC comment: only formats 0b10/0b11 are reachable in encoder\n- Fix test docs: roundtrip tests verify correctness, not specific format selection\n- Rename test to roundtrip_large_literals (accurate scope)\n\n* fix(encoding): replace runtime unreachable with compile-time const assert\n\n- Use `const { assert!(MAX_BLOCK_SIZE <= 262143) }` for compile-time safety\n- Replace `_ => unreachable!()` with `_ => (0b11, 18)` wildcard arm\n- Add assert for alphabet_size > 0 in test helpers\n\nEliminates uncoverable dead code that caused 0% patch coverage.\n\n* fix(encoding): add runtime debug_assert for literals size invariant\n\nGuard against custom Matcher implementations that might produce\nliterals exceeding the 18-bit size format limit (262143 bytes).\n\n* fix(encoding): use release-mode assert for literals size guard\n\nUpgrade debug_assert! to assert! — truncated 18-bit writes in release\nbuilds would produce corrupt streams silently. The assert fires in all\nbuild profiles, preventing invalid output from custom Matcher impls.\n\n* refactor(encoding): move const assert to module scope, add assert message\n\n- Move compile-time MAX_BLOCK_SIZE check to idiomatic `const _: () = assert!(...)`\n  at module scope instead of inline `const { ... }` expression\n- Add static panic message to runtime assert (format args omitted to avoid\n  uncoverable dead code in coverage instrumentation)",
          "timestamp": "2026-03-25T23:58:37+02:00",
          "tree_id": "747d0da8c7a878ee8b94d143fcf834e77c63915a",
          "url": "https://github.com/structured-world/structured-zstd/commit/1c9c616fea994ca4a257d48cfb28c798a5284b7d"
        },
        "date": 1774476016158,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 8.973,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.931,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.129,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.376,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.074,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "98894d01f31dd5444ffeb54194bb99ef280e0918",
          "message": "feat(encoder): FSE table reuse and offset history optimization (#33)\n\n* feat(encoder): FSE table reuse and offset history optimization\n\n- Implement repeat offset encoding (codes 1/2/3) with offset history\n  tracking across blocks, matching decoder's RFC 8878 §3.1.2.5 logic\n- Replace hardcoded FSE table selection with cost-based heuristic that\n  compares new table (with header cost) vs predefined vs previous table\n- Add offset_hist [u32; 3] to CompressState, initialized per RFC 8878\n- Add symbol_probability() and num_symbols() accessors to FSETable\n- Add 5 regression tests covering repeat offsets, zero-ll sequences,\n  multi-block persistence, compression ratio, and C zstd interop\n\nCloses #17\n\n* docs: fix clippy command in copilot-instructions\n\nThe `--all-features` flag includes `rustc-dep-of-std` which pulls in\n`compiler_builtins` — an internal feature for Rust stdlib builds that\nfails on stable. Align with CI which already uses explicit features.\n\n* fix(encoder): align fse repeat state with decoder\n\n- charge exact FSE table header cost in table selection\n- store the actual last-used tables across blocks and reset them per frame\n- add interop and regression coverage for multi-block and reuse cases\n\nCloses #17\n\n* test(encoder): harden fse regressions\n\n- add a narrow regression for remembering last-used FSE tables\n- remove extra copying from reused FrameCompressor coverage\n- fix workspace lint noise in existing test helpers\n\nCloses #17\n\n* test(interop): add reverse fse regression coverage\n\n- cover Huffman-heavy seed=100 in ffi-to-rust direction\n- cover repeat-offset-friendly inputs in ffi-to-rust direction\n\nCloses #17\n\n* test(encoder): add single-symbol table regression\n\n- cover choose_table on a single emitted symbol\n- reproduce the dynamic-table panic before the fix\n\nCloses #17\n\n* fix(encoder): avoid single-symbol fse panic\n\n- skip the dynamic-table path for single-symbol distributions\n- reduce repeat-table persistence overhead for encoded modes\n- cache debug table-header verification in write_table\n\nCloses #17\n\n* docs(encoder): clarify fse cost comparison\n\n- document why choose_table keeps exact cost comparison\n- explain why only the degenerate single-symbol case is short-circuited\n\nCloses #17\n\n* fix(fse): assert table header alignment\\n\\n- document that single-symbol streams stay on predefined/repeat paths until sequence-section RLE exists\\n- assert byte-aligned FSE table header writes and document the matching size contract\\n\\nCloses #17\n\n* fix(encoder): harden repeat table state\\n\\n- avoid none unwrap when repeat mode has no previous table\\n- store previous default/custom tables without cloning full defaults each block\\n- raise crates to edition 2024 and fix resulting clippy/unsafe issues\\n\\nCloses #17\n\n* fix(ringbuffer): restore branchless copy path\\n\\n- wire extend_from_within_unchecked_branchless back to the no-branch helper\\n- keep workspace formatting aligned after the edition-2024 cleanup\\n\\nCloses #17\n\n* style(rustfmt): apply workspace formatting\\n\\n- run cargo fmt --all after the edition-2024 cleanup\\n- align imports and multiline formatting with current rustfmt output\\n\\nCloses #17\n\n* test(coverage): cover edition cleanup paths\n\n* fix(encoder): tighten repeat table reuse\n\n- remove dictionary training dbg! noise from the hot loop\n- keep repeat-mode previous FSE table slots without cloning custom boxes\n- strengthen repeat-offset fixtures so reuse paths require repeated short matches\n- clarify frame decoder wording in display errors\n\n* test(errors): sharpen regression coverage\n\n- fix block type display punctuation\n- split display regression assertions by error family\n- align repeat-offset comparison baselines by input size\n- make cross-block and ll=0 fixtures assert the intended reuse paths\n\n* fix(fse): silence bench-only warnings\n\n- gate debug-only header bit accounting locals behind debug_assertions\n- keep bench and release builds warning-free without weakening checks\n\n* build(rust): pin workspace toolchain to 1.94\n\n* fix(encoder): avoid invalid fse table fallback\n\n* ci(rust): install i686 target explicitly\n\n* ci(rust): pin workflow toolchain to 1.94\n\n* test(errors): lock display regressions to exact text\n\n* docs(ci): clarify toolchain and test intent\n\n* ci(bench): pin benchmark toolchain to 1.94\n\n* ci(rust): follow stable and pin msrv\n\n- switch the default workspace and CI toolchain back to stable\\n- pin the dedicated msrv job to 1.92.0 explicitly\\n- keep rust-version 1.92 in Cargo manifests as the compatibility floor\\n\\nCloses #17\n\n* fix(zstd): tighten wraparound review fixes\n\n- allow exact one-past-end pointer bounds in branchless ringbuffer writes\\n- keep the wraparound regression helper on real split layouts\\n- remove frame-overhead bias from the multi-block reuse size assertion\\n- document why the FSE cost model keeps the shared entropy estimate\\n\\nCloses #17\n\n* test(zstd): stabilize review-driven assertions\n\n- document the single-symbol fallback table workaround\\n- weaken the repetitive-vs-random compression assertion to a stable ordering check\\n\\nCloses #17\n\n* test(zstd): narrow zero-ll coverage claim\n\n- align the zero literal-length regression comments with the fixture actually exercised end-to-end\\n\\nCloses #17",
          "timestamp": "2026-03-26T17:15:14+02:00",
          "tree_id": "60699f79a9f24cae5888463ba3f2551cca58ca79",
          "url": "https://github.com/structured-world/structured-zstd/commit/98894d01f31dd5444ffeb54194bb99ef280e0918"
        },
        "date": 1774538263638,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9.044,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.91,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.51,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.47,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.124,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b1096acbdf7b56e0acbeaf7e32bcf18296aed958",
          "message": "ci(release): restore release-plz lineage (#36)\n\n* ci(release): restore release-plz lineage\n\n- switch release-plz to git-only fork tags\n\n- bump release-plz/action and fix workspace packaging inputs\n\n- prepare bootstrap tag state for the fork release history\n\nCloses #35\n\n* fix(zstd): make crate docs package-safe\n\n* fix(zstd): make crate README package-safe\n\n* fix(zstd): keep crate README as symlink\n\n* fix(zstd): correct crate README symlink casing\n\n* fix(release): stabilize crate readme wiring\n\n* ci(release): reclaim v-tag lineage",
          "timestamp": "2026-03-26T22:46:49+02:00",
          "tree_id": "44fefbdff9b013331970908a76f2727a76522ef8",
          "url": "https://github.com/structured-world/structured-zstd/commit/b1096acbdf7b56e0acbeaf7e32bcf18296aed958"
        },
        "date": 1774558113394,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 8.893,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.916,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.431,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.354,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.097,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "9c718d5e8ff3dd6d17d6d06148722cd2a8fa9c84",
          "message": "fix(encoding): implement default compression level (#34)\n\n* fix(encoding): implement default compression level\n\n- route CompressionLevel::Default through the encoder\n- configure matcher window for default-level frames\n- add roundtrip and ffi regression coverage\n\nCloses #5\n\n* fix(encoding): tighten default matcher review follow-ups\n\n- preserve matcher constructor baselines across reset\n- document Default as matching Fastest behavior today\n- add multi-block default roundtrip regression\n\nCloses #5\n\n* docs(encoding): clarify explicit level config arms\n\n- explain why level_config keeps explicit match arms\n- document that current levels share the baseline matcher config\n\nCloses #5\n\n* feat(encoding): implement dfast default level\n\n* perf(encoding): incrementalize dfast matcher\n\n* test(encoding): cover dfast matcher dispatch\n\n* test(encoding): document dfast matcher tradeoffs\n\n* docs(encoding): clarify dfast review invariants\n\n* fix(encoding): tighten review follow-ups",
          "timestamp": "2026-03-26T23:22:23+02:00",
          "tree_id": "8d707f85547f63ed965378e355c8543330bdaf31",
          "url": "https://github.com/structured-world/structured-zstd/commit/9c718d5e8ff3dd6d17d6d06148722cd2a8fa9c84"
        },
        "date": 1774560252934,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 8.977,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.911,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.512,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.377,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.169,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "bd7b94196acba7839ba3c7558448784ea24ddcc0",
          "message": "chore: release v0.0.3 (#37)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-03-26T23:39:42+02:00",
          "tree_id": "f799531ab02485ae16652eea7013df0cc8800306",
          "url": "https://github.com/structured-world/structured-zstd/commit/bd7b94196acba7839ba3c7558448784ea24ddcc0"
        },
        "date": 1774561286105,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 8.979,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.922,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 18.432,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 3.388,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.106,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "5ca2e4e977ff490239ad2ef0cc1c587588923ebc",
          "message": "perf(encoding): align fastest matcher with zstd fast path (#39)\n\n* perf(encoding): align fastest matcher with zstd fast path\n\n* style(encoding): satisfy fmt and clippy for matcher\n\n* fix(encoding): resolve matcher review feedback\n\n- fix repcode offset validation across full searchable window\n- make word-wise prefix counting endian-safe on big-endian targets\n- strengthen matches() invariants and add cross-slice repcode regression test\n\n* test(encoding): improve patch coverage for matcher fixes\n\n- split endian mismatch-byte logic with compile-time cfg helpers\n- add regression for out-of-range repcode offset rejection\n\n* fix(encoding): probe all zero-literal repcode candidates\n\n- check rep1, rep2, and rep0-1 when literals_len is zero\n- deduplicate repeat offsets before probing to avoid duplicate work\n- add regression test covering rep2 fallback when rep1 misses\n\n* fix(encoding): preserve boundary anchors with stepped hash fill\n\n- backfill the last searchable suffix anchor in add_suffixes_till\n- force full-position seeding for skip_matching blocks\n- split zero-literal repcode tests into explicit rep2 and rep0-1 cases",
          "timestamp": "2026-03-27T09:45:19+02:00",
          "tree_id": "98f4c2dadc34735044b6dc08d4ca8e9c7ed0f69a",
          "url": "https://github.com/structured-world/structured-zstd/commit/5ca2e4e977ff490239ad2ef0cc1c587588923ebc"
        },
        "date": 1774597632761,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "decompress/pure_rust",
            "value": 9,
            "unit": "ms"
          },
          {
            "name": "decompress/c_ffi",
            "value": 2.915,
            "unit": "ms"
          },
          {
            "name": "compress/pure_rust/fastest",
            "value": 17.576,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level1",
            "value": 2.769,
            "unit": "ms"
          },
          {
            "name": "compress/c_ffi/level3",
            "value": 5.114,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f3310cb31f631b79fe5536ad155d1debc3ac623d",
          "message": "test(bench): expand zstd benchmark suite (#38)\n\n* test(bench): expand zstd benchmark suite\n\n- add scenario-based Criterion matrix against C zstd\n\n- generate benchmark JSON and markdown reports for CI\n\n- document benchmark workflows and add flamegraph helper\n\nRefs #24\n\n* docs(readme): add benchmark dashboard link\n\n* fix(bench): harden matrix scripts and edge scenarios\n\n- guard ratio reporting for zero-length inputs\n- skip empty Silesia fixtures with BENCH_WARN\n- use mktemp + trap for raw bench output parsing\n- make flamegraph --root opt-in via BENCH_FLAMEGRAPH_USE_ROOT\n\n* fix(bench): tighten flamegraph and decode benchmarks\n\n* docs(bench): clarify decode benchmark asymmetry rationale\n\n* perf(bench): remove redundant decode buffer fill\n\n* fix(bench): scope large default to CI and enforce ratio rows\n\n* feat(bench): add memory and dictionary benchmark reporting\n\n* test(bench): align decompression benchmark paths\n\n- include scenario id in ratio report markdown table\n\n- reuse decoders and buffers in decompression benchmark loops\n\n- keep throughput comparison focused on decode work\n\nRefs #24\n\n* test(bench): include scenario ids in report tables\n\n- add explicit Label columns for memory and dictionary sections\n\n- render stable scenario ids instead of labels in Scenario column\n\nRefs #24\n\n* fix(bench): guard dictionary ratio division\n\n- handle zero-length scenario inputs in emit_dictionary_report\n\n- emit 0.0 ratios instead of inf/NaN for empty payload edge cases\n\nRefs #24\n\n* fix(bench): bound Silesia fixture loading\n\n- cap loaded fixture count and file size for predictable startup\n\n- support STRUCTURED_ZSTD_SILESIA_MAX_FILES and _MAX_FILE_BYTES overrides\n\n- emit BENCH_WARN diagnostics when limits are applied\n\nRefs #24\n\n* docs(bench): clarify memory estimates in reports\n\n- rename REPORT_MEM fields to buffer-bytes estimate names\n\n- update report section/columns to explicit input+output estimates\n\n- sync README and BENCHMARKS wording with new semantics\n\nRefs #24\n\n* perf(bench): cache benchmark scenario generation\n\n- build scenario inputs once via OnceLock\n\n- reuse cached slice across compress/decompress/dictionary benches\n\nRefs #24\n\n* chore(bench): drop unused stats_alloc dep\n\n- remove unused dev-dependency from zstd/Cargo.toml\n\nRefs #24\n\n* fix(bench): allow filtered runs without dict rows\n\n- downgrade missing REPORT_DICT from error to warning\n\nRefs #24\n\n* fix(bench): avoid duplicate dict fallback samples\n\n- use single-sample fallback for tiny dictionary training inputs\n\n- emit BENCH_WARN when fallback path is used\n\nRefs #24\n\n* style(bench): add is_empty for Scenario\n\n- satisfy len_without_is_empty expectations for bench helper type\n\nRefs #24\n\n* fix(bench): remove needless borrows in scenario loops\n\n- pass cached scenario references directly in helper calls\n\n- keep clippy clean with OnceLock-backed scenario cache\n\nRefs #24\n\n* fix(bench): sanitize Silesia scenario report fields\n\n- normalize Silesia-derived scenario ids to safe ASCII tokens\n\n- escape report labels before emitting REPORT/REPORT_MEM/REPORT_DICT lines\n\nRefs #24\n\n* perf(bench): bound Silesia dir walk by max_files\n\n- stop collecting file paths once fixture limit is reached\n\n- keep deterministic ordering by sorting only the bounded subset\n\nRefs #24\n\n* build(bench): ship decode corpus fixture in crate\n\n- keep include_bytes corpus scenario available in packaged bench sources\n\n- remove decodecorpus_files from zstd crate exclude list\n\nRefs #24\n\n* fix(bench): avoid packaging decode corpus fixtures\n\n- restore decodecorpus_files exclusion for crate packaging size\n\n- load corpus sample at runtime with synthetic fallback when fixture is absent\n\nRefs #24\n\n* fix(bench): parse escaped labels in report script\n\n- accept backslash-escaped quotes in REPORT label regexes\n\n- unescape parsed labels before markdown rendering\n\nRefs #24\n\n* fix(bench): pass criterion filter correctly to flamegraph\n\n- remove unsupported --bench flag from benchmark binary arguments\n\nRefs #24\n\n* style(bench): format runtime corpus loader\n\n- apply rustfmt after decodecorpus runtime load changes\n\nRefs #24\n\n* fix(bench): stabilize corpus fallback scenarios\n\n- use distinct scenario id/label when decode corpus fixture is unavailable\n\n- collect, sort, and truncate Silesia files deterministically\n\n* fix(bench): gate report precompute and escape labels\n\n- emit REPORT* lines only when STRUCTURED_ZSTD_EMIT_REPORT is enabled\n\n- set report env var in run-benchmarks workflow\n\n- escape markdown table cell labels in benchmark-report.md generation\n\n* fix(bench): harden silesia fixture identity and size checks\n\n- compare metadata.len() against max size in u64 space\n\n- derive Silesia scenario ids from full file names\n\n- append stable numeric suffix on id collisions\n\n* fix(bench): tighten label escaping and id dedupe guard\n\n- expand markdown table escaping for benchmark labels\n\n- bound scenario id suffix search and panic deterministically on exhaustion",
          "timestamp": "2026-03-28T15:29:19+02:00",
          "tree_id": "b8bb55741bf22a0657b8e6948f8c90ccf87df0d1",
          "url": "https://github.com/structured-world/structured-zstd/commit/f3310cb31f631b79fe5536ad155d1debc3ac623d"
        },
        "date": 1774705214958,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 6.473,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.207,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.779,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 6.175,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.483,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.776,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 118.502,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.095,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.665,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.391,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 193.496,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.447,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.478,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.181,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.303,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.234,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 24.732,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.74,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 99.448,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.424,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/matrix/pure_rust",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 5.179,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 5.293,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/matrix/pure_rust",
            "value": 3.133,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/matrix/c_ffi",
            "value": 0.778,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/matrix/pure_rust",
            "value": 3.151,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/matrix/c_ffi",
            "value": 0.778,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.493,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.723,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.6,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.438,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "6fc1fc34dadcbb7f5b72466bebf15fb01e6330ec",
          "message": "perf(encoding): interleave fastest hash fill insertion (#41)\n\n* perf(encoding): interleave fastest hash fill insertion\n\n- add interleaved suffix insertion path for fill-step=3 in matcher\n\n- keep tail-anchor backfill behavior unchanged\n\n* chore(repo): ignore generated benchmark reports\n\n- add benchmark-report.md and benchmark-results.json to gitignore\n\n* fix(review): dedupe ignores and guard suffix bounds\n\n- remove duplicate benchmark artifact entries in .gitignore\n\n- add debug_assert for insert_suffix_if_absent slice bound invariant\n\n* test(matcher): cover interleaved fill boundaries\n\n- add regression for idx < MIN_MATCH_LEN in add_suffixes_till\n\n- add focused interleaved-position registration test\n\n- fix insert_limit saturation to preserve original windows() behavior",
          "timestamp": "2026-03-28T17:58:13+02:00",
          "tree_id": "8a7010bb7ed0b4af686503fbb3cb1f1da75087c4",
          "url": "https://github.com/structured-world/structured-zstd/commit/6fc1fc34dadcbb7f5b72466bebf15fb01e6330ec"
        },
        "date": 1774714144506,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.792,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.21,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.414,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.798,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.881,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.713,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 116.694,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.881,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.148,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.253,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 191.844,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.33,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.18,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.887,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.233,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.269,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.731,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 96.788,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.367,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 5.215,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 5.291,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/matrix/pure_rust",
            "value": 0.182,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/matrix/pure_rust",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/matrix/pure_rust",
            "value": 2.928,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/matrix/c_ffi",
            "value": 0.652,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/matrix/pure_rust",
            "value": 2.947,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/matrix/c_ffi",
            "value": 0.648,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.462,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.72,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.528,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.367,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "4857f1e396c6c3f438e2749afd9aae7261d5e4e6",
          "message": "perf(decoding): optimize sequence execution with overlap fast paths (#42)\n\n* perf(decoding): optimize sequence match execution paths\n\n- split repeat into overlap-aware paths (>=16, >=8, <8)\n\n- add short-offset replication path with chunked pattern writes\n\n- add match/literal/dictionary prefetch hints for x86/x86_64\n\n- add regression test covering overlap fast-path correctness\n\nRefs #12\n\n* fix(decoding): harden offset handling and wraparound coverage\n\n- return DecodeBufferError::ZeroOffset for repeat(offset=0)\n\n- add wrapped-ringbuffer overlap regression coverage\n\n- move prefetch helper into shared decoding::prefetch module\n\nRefs #12\n\n* test(decoding): cover zero-offset and tune short-repeat path\n\n- add regression for repeat(offset=0) -> DecodeBufferError::ZeroOffset\n\n- switch short-offset repeat hot loop to 8-byte phase patterns\n\n- gate x86 prefetch by SSE and remove unnecessary unsafe\n\nRefs #12\n\n* fix(decoding): restore safe prefetch intrinsic usage\n\n- wrap _mm_prefetch calls in unsafe blocks for target-feature safety\n\n- keep x86 SSE gating and no-op fallback intact\n\nRefs #12\n\n* fix(decoding): advance output counter on full dict copy\n\n- update total_output_counter when bytes_from_dict >= match_length\n\n- add regression test for stale-window guard after full dict branch\n\nRefs #12",
          "timestamp": "2026-03-28T20:36:50+02:00",
          "tree_id": "2067d73a42ca36d0e02ffd37717a7fe25992b37f",
          "url": "https://github.com/structured-world/structured-zstd/commit/4857f1e396c6c3f438e2749afd9aae7261d5e4e6"
        },
        "date": 1774723656060,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 4.978,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.215,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.012,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.458,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.678,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.118,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.73,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 113.208,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.823,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.149,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.255,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 187.519,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.397,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.329,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.195,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.807,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.572,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 102.676,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 4.683,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 0.964,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 4.926,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/matrix/pure_rust",
            "value": 0.313,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/matrix/pure_rust",
            "value": 3.108,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/matrix/c_ffi",
            "value": 0.723,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/matrix/pure_rust",
            "value": 3.118,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/matrix/c_ffi",
            "value": 0.733,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.476,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.735,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.541,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.423,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "29de24a34bf05c32e3a64b6025c69b3a90cafd48",
          "message": "test(bench): expand benchmark parity matrix (#43)\n\n* test(bench): expand benchmark parity matrix\n\n- benchmark decompression on both rust_stream and c_stream inputs\n- document issue #24 acceptance mapping and matrix behavior\n- ignore accidental root artifact file '='\n\nCloses #24\n\n* test(bench): harden decompress benchmark checks\n\n- validate decoded bytes against scenario payload before timing\n- apply black_box to decompression input/output paths\n- fix flamegraph filter example for source-segmented benchmark names",
          "timestamp": "2026-03-28T21:35:45+02:00",
          "tree_id": "3de95daf36b2528d7b8368485894c9ef6e959aa2",
          "url": "https://github.com/structured-world/structured-zstd/commit/29de24a34bf05c32e3a64b6025c69b3a90cafd48"
        },
        "date": 1774727437573,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.803,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.019,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.207,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.28,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.031,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.638,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.397,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.743,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 111.941,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.77,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.527,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.267,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 186.785,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.374,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.17,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.47,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.197,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.47,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.591,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 90.962,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.263,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.491,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.621,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.963,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.913,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.504,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.906,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.114,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.176,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.036,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.321,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.301,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.788,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.933,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.915,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.657,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.691,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.805,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.921,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.481,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.754,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.488,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.305,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "13b08662a83188a21acaa822c94b9e9248ab1587",
          "message": "feat(encoding): add dictionary compression support (#44)\n\n* feat(encoding): add dictionary compression support\n\n- add FrameCompressor dictionary APIs, including parse-from-bytes helper\n- write dictionary id into frame header and prime matcher with dictionary history\n- support raw-content dictionaries for dict_builder outputs\n- add regression tests for dict-id enforcement, C interop, and dict_builder roundtrip\n\nCloses #8\n\n* fix(encoding): harden dictionary input handling\n\n- reject dictionary id 0 in FrameCompressor::set_dictionary\n\n- return explicit DictionaryDecodeError on undersized dictionary buffers\n\n- keep dict_tests assets in crate package so include_bytes tests compile downstream\n\n* style(tests): format dictionary panic regression asserts\n\n* fix(dict): align defaults and clarify dictionary priming\n\n- use RFC default repeat offsets [1,4,8] in decode_dict initialization\n\n- document intentional dual offset history priming in compressor state and matcher\n\n- document fail-fast zero-id contract for set_dictionary\n\n* test(packaging): handle must_use and trim crate payload\n\n- assert first set_dictionary_from_bytes insert returns None\n\n- explicitly discard optional previous dictionary in dict_builder roundtrip test\n\n- exclude dict_tests/files/** while keeping dict_tests/dictionary for include_bytes tests\n\n* fix(dict): seed encoder entropy and reject zero repcodes\n\n- restore previous Huffman/FSE encoder tables from parsed dictionaries before first block\n\n- convert decoder-side entropy tables into encoder tables for dictionary priming\n\n- reject zero repeat offsets during dictionary parsing with explicit decode error\n\n- add regression tests for entropy seeding and zero-repeat-offset rejection\n\n* perf(encoding): cache dictionary entropy tables\n\n- precompute decoder->encoder entropy conversions when dictionary is set\n\n- reuse cached tables across compress() calls to avoid per-frame rebuild\n\n- keep explicit fail-fast comment for zero dictionary id API contract\n\n- derive Clone for encoder HuffmanTable to support cache reuse\n\n* fix(encoding): harden dictionary priming suffix reuse\n\n* fix(encoding): harden dictionary priming invariants\n\n* fix(encoding): validate repcodes and retain dict history\n\n* test(encoding): avoid moving frame decode error in assert\n\n* test(encoding): cover dfast priming and decouple header window\n\n* chore(encoding): document len_log zero guard\n\n* fix(encoding): cap dictionary retention to committed bytes\n\n* style(encoding): format tail budget subtraction\n\n* fix(encoding): skip dictionary state for raw frames\n\n* fix(encoding): address renewed review threads\n\n* test(encoding): cover window crossing with dictionaries\n\n* fix(encoding): gate dictionary id on matcher capability\n\n- add Matcher::supports_dictionary_priming with safe default false\n- enable dictionary state only for matchers that opt in\n- harden window regression test against no-dictionary baseline\n- add regression test for custom matcher without dictionary priming\n\n* fix(encoding): retire dictionary budget on eviction\n\n- clarify set_dictionary docs for uncompressed and non-priming matchers\n- track retained dictionary budget separately from advertised live window\n- shrink matcher capacity as primed dictionary bytes are evicted\n- add regression tests for simple and dfast budget retirement\n\n* perf(encoding): avoid extra entropy-table cloning\n\n- seed huffman table directly via Option::clone_from from cached entropy\n- cache FSE previous tables as PreviousFseTable to avoid per-frame reboxing\n- remove temporary clone/map allocations in dictionary seeding path\n\n* fix(encoding): correct dfast eviction accounting\n\n- keep dfast eviction callbacks on logical slice length, not vec capacity\n- add regression tests for add_data/trim_to_window eviction length semantics\n- remove intermediate Option clones in FSE dictionary seeding path\n\n* fix(encoding): preserve entropy tables for clone_from reuse\n\n* fix(dictionary): reject empty raw dictionaries",
          "timestamp": "2026-03-29T19:52:35+03:00",
          "tree_id": "85e82bc44e736a44cb392028deb7837612a3a1e8",
          "url": "https://github.com/structured-world/structured-zstd/commit/13b08662a83188a21acaa822c94b9e9248ab1587"
        },
        "date": 1774804046067,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 6.903,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.211,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 8.74,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 6.783,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.022,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.34,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.76,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 119.974,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.915,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.751,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 205.526,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.439,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.194,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 13.442,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.234,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 24.312,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.631,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 108.824,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.246,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.551,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.538,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.981,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.964,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.514,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.895,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.058,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.119,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.185,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.596,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.929,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.086,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.657,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.76,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.811,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.872,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.492,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.785,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.429,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.282,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "db372df0d04d6d67d378fd386e45404f3143c977",
          "message": "chore: release v0.0.4 (#40)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-01T09:06:11+03:00",
          "tree_id": "dd083df491bcaa24faf8f73d9f758d313e18ee7f",
          "url": "https://github.com/structured-world/structured-zstd/commit/db372df0d04d6d67d378fd386e45404f3143c977"
        },
        "date": 1775024484737,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.022,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.778,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.212,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.661,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.033,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.715,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.962,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.697,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 133.338,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.763,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.955,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.276,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 228.061,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.323,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.403,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.189,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.123,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.242,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 24.205,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.757,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 97.654,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.383,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.464,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.441,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.963,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.906,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.504,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.76,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.033,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.189,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.12,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.524,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.196,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.191,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.192,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.055,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.32,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.894,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 2.03,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.016,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.66,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.964,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.877,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.981,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.662,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.458,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.722,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.457,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.296,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "8f2fe9b91e446479676986b6c836d1da81c595e3",
          "message": "feat(encoding): add Better compression level (zstd level 7, lazy2 strategy) (#48)\n\n* feat(encoding): implement Better compression level with hash-chain matcher\n\nAdd CompressionLevel::Better — roughly equivalent to zstd level 7.\n\n- Hash table (1M u32) + chain table (512K) with 16-candidate traversal\n- Gain-based lazy2 matching: match_len*4 - log2(offset) cost model\n- Positions stored as abs_pos+1 with 0 sentinel (safe at 4 GiB boundary)\n- Boundary backfill and tail insertion for cross-slice match continuity\n- 8 MiB window, 6 MiB tables, ~22 MiB peak memory\n- StreamingEncoder buffer reuse, CLI level 3, benchmark suite integration\n\nRatio on corpus z000033: 541K vs Default 578K (+6.3%), C zstd L7 510K\n\nCloses #6\n\n* refactor(encoding): rename compress_fastest to compress_block_encoded\n\nFunction serves Fastest, Default, and Better — old name was misleading.\nAlso update CompressionLevel::Better doc and clean up stale Dfast comments.\n\n* test(encoding): add Better level roundtrip, ratio, and window regression tests\n\n- 10 roundtrip tests: compressible, random, multi-block, streaming, edge cases\n- Assert Better < Default on corpus proxy and 6 MiB repeating data\n- Cross-validation: Rust Better compress -> C FFI decompress\n- Fix rustfmt import ordering\n\n* fix(encoding): guard u32::MAX sentinel collision and chain self-loops\n\n- Skip insert_position at abs_pos == u32::MAX (stored+1 would wrap to HC_EMPTY)\n- Detect chain self-loops from masked chain_idx collisions (512K periodicity)\n- Rewrite large-window test with explicit two-region layout and compressible gap\n\n* fix(encoding): guard abs_pos overflow before u32 cast, deduplicate streaming helper\n\n- Check abs_pos >= u32::MAX as usize before casting (prevents silent truncation)\n- Delegate roundtrip_streaming to roundtrip_streaming_at_level for consistency\n- Fix import ordering (cargo fmt)\n\n* docs(encoding): note 4 GiB u32 position limit on Better level (#51)\n\n- CompressionLevel::Better doc warns about >4 GiB single-frame degradation\n- TODO(#51) at insert_position guard for future table rebasing\n- Clarify min_primed_tail 4-byte requirement for HC backend\n\n* refactor(encoding): extract MAX_CHAIN_STEPS constant from magic multiplier\n\n* test(encoding): add HC priming regression tests and refine u32 doc\n\n- Add hash-chain dictionary priming test (mirrors Dfast variant)\n- Add HC retention budget eviction test\n- Refine Better doc comment: u32 limit is ~4 GiB + window, not instant",
          "timestamp": "2026-04-02T17:58:09+03:00",
          "tree_id": "e53f07eecad67779da5b523b52ee6fddc3b083ac",
          "url": "https://github.com/structured-world/structured-zstd/commit/8f2fe9b91e446479676986b6c836d1da81c595e3"
        },
        "date": 1775143197246,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.121,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.141,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.086,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.217,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.012,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.461,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.505,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.099,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.989,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.121,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.076,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.702,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.663,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 107.386,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.676,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 52.89,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.227,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.169,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.234,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 173.268,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.287,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 62.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.578,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.372,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.32,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.236,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.38,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.629,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.514,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.134,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 91.296,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.787,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 77.273,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.202,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.516,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.608,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.479,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.962,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.938,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.502,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.822,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.617,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.708,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.993,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.178,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.191,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.049,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.304,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.585,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.922,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.738,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.653,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.531,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.838,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.705,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.653,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.846,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.793,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.746,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.655,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.102,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.436,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.715,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.361,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.243,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.19,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.26,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "d4023152f62e97938f7268c84384f2e6da6424f9",
          "message": "feat(encoding): add Best compression level (zstd level 11, btlazy2 strategy) (#53)\n\n* feat(encoding): add Best compression level (zstd level 11, btlazy2 strategy)\n\n- Implement Best level using hash-chain matcher with deep lazy2 matching:\n  16 MiB window, 2M/1M hash/chain tables, 32-candidate search depth,\n  128 target match length\n- Make HC matcher parameters (hash_log, chain_log, search_depth,\n  target_len) configurable per level via new configure() method\n- Add Best to streaming encoder, frame compressor, and CLI (level 4)\n- Add 11 roundtrip integrity tests covering compressible, random,\n  multi-block, streaming, edge cases, repeat offsets, and large window\n- Add cross-validation: Rust Best → C FFI decompress, ratio assertion\n  (Best beats Better on corpus proxy)\n- Add Best to benchmark level matrix (vs C zstd level 11)\n- Update README checklist and BENCHMARKS level mapping\n\nCloses #7\n\n* fix: remove unreachable pattern in frame_compressor\n\nAll CompressionLevel variants are now covered exhaustively after\nadding Best; the wildcard arm was dead code producing a warning.\n\n* style: apply cargo fmt\n\n* fix(encoding): clamp HC search_depth and harden Best-level tests\n\n- Clamp search_depth to MAX_HC_SEARCH_DEPTH in configure() to prevent\n  OOB panic if a future level config exceeds the fixed-size array\n- Strict assertion in best_beats_better test (< not <=) to catch\n  accidental Best/Better aliasing\n- Increase best_level_streaming_roundtrip payload to 200 KiB to cross\n  the 128 KiB block boundary\n- Document ensure_level_supported exhaustive guard and chain_candidates\n  fixed-size array design\n\n* test(encoding): use high-entropy region in large-window test\n\nUse generate_data (random) instead of generate_compressible for the\nduplicated region so the size win depends on window reach, not\nintra-region compression.\n\n* docs(encoding): remove platform-specific byte count from array doc\n\nThe stack array size depends on pointer width; \"256 bytes\" is only\ncorrect on 64-bit targets.\n\n* style: apply cargo fmt to import order\n\n* refactor(encoding): introduce HcConfig struct and level_roundtrip_suite macro\n\n- Replace positional configure(hash_log, chain_log, search_depth,\n  target_len) with configure(HcConfig), eliminating parameter-order\n  hazard. Add HC_CONFIG and BEST_HC_CONFIG const presets.\n- Extract level_roundtrip_suite! macro that generates the standard 7-test\n  roundtrip suite (compressible, random, multi-block, streaming, edge\n  cases, repeat offsets, large literals) for any compression level.\n  Better and Best now share the same test matrix via module-scoped\n  macro invocations.\n\n* fix(encoding): release oversized HC tables when switching away from Best\n\nWhen MatchGeneratorDriver switches from HashChain to another backend,\ndrop the hash/chain tables so Best's larger 2M/1M allocations don't\npersist across frames that use Default or Fastest.\n\n* test(encoding): cover HC table release and resize on level switch\n\n- driver_best_to_fastest_releases_oversized_hc_tables: verifies HC\n  hash/chain tables are freed when switching from Best to Fastest\n- driver_better_to_best_resizes_hc_tables: verifies tables grow when\n  switching from Better (1M/512K) to Best (2M/1M)\n\n* fix(encoding): correct Best doc fallback and remove redundant guard\n\n- Best doc: point to Default (not Better) for >4 GiB streams since\n  Better also uses HC with u32 positions\n- Remove redundant `if backend != HashChain` check inside the\n  `active_backend != backend` branch where it is always true\n\n* test(encoding): clarify Best-vs-Better ratio test as non-regression bound\n\nRename to best_level_does_not_regress_vs_better and keep <= assertion:\non this repetitive fixture HC finds identical matches at any depth\n(best == better == 30243 bytes). The strict < check lives in\ncross_validation::best_level_beats_better_on_corpus_proxy where the\nmore diverse decodecorpus sample differentiates the levels.",
          "timestamp": "2026-04-03T02:05:23+03:00",
          "tree_id": "b63fd60cf5db6e3b48c38f980f10bbc70f9eb9f7",
          "url": "https://github.com/structured-world/structured-zstd/commit/d4023152f62e97938f7268c84384f2e6da6424f9"
        },
        "date": 1775172856330,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.094,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.107,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.289,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.213,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.535,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.572,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.675,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.376,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.312,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.019,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.124,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.077,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.273,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.743,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.692,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 103.641,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.654,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 55.988,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.275,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.668,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.425,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.059,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 174.011,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.289,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 66.535,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.596,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 56.871,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 0.904,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.371,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.179,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.686,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.749,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.585,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.782,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.048,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.555,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.688,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 100.563,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.203,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 77.67,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.052,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 78.782,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 15.235,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.465,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.612,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.424,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.964,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.896,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.749,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.033,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.569,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.713,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.618,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.063,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.51,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.613,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.495,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.966,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.177,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.181,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.432,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.432,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.305,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.316,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.305,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.316,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.699,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.919,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.822,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.485,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.786,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.7,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.684,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.791,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.822,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.678,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.792,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.039,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.663,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.103,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.332,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.461,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.718,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.326,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.17,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.254,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.394,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.471,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 36.943,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "711594535955efd8017d04b650a14bb0cf83b201",
          "message": "perf(decoding): dual-state interleaved FSE sequence decoding (#55)\n\n* perf(decoding): dual-state interleaved FSE sequence decoding\n\n- Add ensure_bits() and get_bits_unchecked() to BitReaderReversed for\n  batched unchecked bit reads after a single refill check\n- Add update_state_fast() to FSEDecoder using unchecked reads\n- Restructure both sequence decode loops (with/without RLE) to use one\n  ensure_bits() call covering all three FSE state updates per iteration,\n  replacing three individual per-update refill checks\n- Fix pre-existing bench compile error (rand 0.10 Rng -> RngExt)\n\nCloses #11\n\n* style: fix rustfmt import ordering in bench support\n\n* test(decoding): add debug assertions and fast-path bit reader test\n\n- Add debug_assert!(n <= 56) to get_bits_unchecked\n- Add debug_assert!(max_update_bits <= 56) in both sequence decode loops\n- Add ensure_and_unchecked_match_get_bits test covering fast-path\n  equivalence with get_bits across refill boundaries and n=0 edge case\n- Update bench rand 0.10 doc: Rng::fill() → RngExt::fill()\n\n* docs(decoding): clarify batched refill comments in sequence decoder\n\nRemove misleading \"dual-state interleaving\" wording from comments.\nThe optimization is a batched refill check covering three single-state\nFSE decoders, not a dual-state interleaving pattern.\n\n* test(decoding): strengthen get_bits_unchecked precondition and test coverage\n\n- Add debug_assert!(bits_consumed + n <= 64) to get_bits_unchecked\n  to catch caller violations in debug builds\n- Force real refill boundary in test: consume 39 bits before batched\n  ensure_bits(26), triggering actual refill (39+26=65 > 64)",
          "timestamp": "2026-04-03T12:11:13+03:00",
          "tree_id": "45165440140c3d2fbfe2ba953f254b884d7db1ab",
          "url": "https://github.com/structured-world/structured-zstd/commit/711594535955efd8017d04b650a14bb0cf83b201"
        },
        "date": 1775209218712,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.734,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.171,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.104,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.288,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.32,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.211,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.052,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.584,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.096,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.657,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.296,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.669,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.151,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.097,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.255,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.361,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.58,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.747,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 111.477,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.741,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 57.701,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.329,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 63.85,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.702,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.784,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 171.4,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.339,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 70.105,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.651,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 60.547,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 1.021,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.347,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.142,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.02,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.235,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.991,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.628,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.173,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.171,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.332,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.148,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 107.822,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.908,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 77.319,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.192,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 77.452,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 12.592,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.238,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.454,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.961,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.75,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.677,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.033,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.339,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.624,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.564,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.992,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.3,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.484,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.967,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.177,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.11,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.182,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.182,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.182,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.307,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.697,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.912,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.735,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.655,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.505,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.79,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.695,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.746,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.787,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.838,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.735,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.788,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.068,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.659,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.106,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.18,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.333,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.455,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.722,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.427,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.31,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.167,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.218,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.463,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 37.03,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "aa92d4f9adebd0c723c73733644cb5ea3684c1a6",
          "message": "perf(decoding): branchless bitstream reader with mask table and BMI2 support (#58)\n\n* perf(decoding): branchless bitstream reader with mask table and BMI2 support\n\n- Add pre-computed BIT_MASK[65] lookup table replacing per-call\n  (1u64 << n) - 1 computation on the hot decode path\n- Add mask_lower_bits() helper with #[cfg(target_feature = \"bmi2\")]\n  fast path using _bzhi_u64 single-cycle instruction\n- Make peek_bits() branchless: remove n==0 guard, use wrapping_shr\n  to handle the bits_consumed=0 edge case without branching\n- Make peek_bits_triple() branchless: same wrapping_shr pattern,\n  mask table for all three sub-values\n- Fix get_bits_triple() to use conditional ensure_bits() instead of\n  unconditional refill(), avoiding redundant work when the bit\n  container already holds enough bits\n- Add bitstream microbenchmark (criterion) covering sequential reads,\n  FSE-pattern triple reads, and ensure+unchecked batched reads\n- Add unit tests for mask table correctness, mask_lower_bits edge\n  cases, peek_bits(0) invariant, and get_bits_triple equivalence\n\nCloses #13\n\n* style: fix rustfmt import ordering in bitstream bench\n\n* fix(bit_io): tighten module visibility and add debug assertions\n\n- Revert `bit_io` to private module — only `BitReaderReversed` is\n  re-exported through `testing` for benchmarks, nothing else leaks\n- Add `debug_assert!(n <= 64)` to `mask_lower_bits` for consistent\n  bounds checking across BMI2 and fallback paths\n- Add `debug_assert!(bits_consumed + n <= 64)` to `peek_bits` and\n  `peek_bits_triple` so invalid caller state is caught in debug\n  builds despite the branchless wrapping arithmetic\n- Switch benchmark throughput from `Throughput::Bytes` to\n  `Throughput::Elements` to accurately report actual reads per\n  iteration rather than overstating by counting unread trailing bits\n\n* fix(bit_io): gate benchmark re-exports behind bench_internals feature\n\n- Add `bench_internals` feature flag (default off) to Cargo.toml\n- Gate `pub mod testing` and `pub use BitReaderReversed` behind\n  `#[cfg(feature = \"bench_internals\")]` so normal builds keep the\n  type fully crate-private\n- Add `required-features = [\"bench_internals\"]` to the bitstream\n  bench target\n- Add `debug_assert_eq!(sum, n1 + n2 + n3)` in `peek_bits_triple`\n  to catch mismatched width arguments in debug builds\n\n* test(bit_io): cover no-refill fast path in get_bits_triple\n\nPrime both readers with 8 bits so the subsequent triple read (26 bits)\nfits within the already-loaded container. This exercises the\nensure_bits() skip-refill path introduced by the conditional refill\nchange.\n\n* fix(bit_io): resolve infinite loop in peek_bits_zero test\n\n- Set bits_consumed = 0 directly instead of looping via get_bits\n- Document mask_lower_bits caller contract (n <= 56)\n- Enable bench_internals feature in CI clippy step\n\n* fix(bit_io): promote mask_lower_bits bounds check to assert and split CI clippy\n\n- Replace debug_assert!(n <= 64) with assert! for consistent panic\n  behavior across BMI2 and non-BMI2 targets in release builds\n- Restore original CI clippy gate (hash,std,dict_builder only)\n- Add separate \"Clippy (bench_internals)\" step for feature-gated code\n\n* test(bit_io): pin n=64 mask boundary and mixed zero-width triple\n\n- Assert mask_lower_bits(x, 64) returns x unchanged (BIT_MASK[64] path)\n- Assert get_bits_triple(5, 0, 4) matches individual reads with zero width\n\n* fix(bit_io): use wrapping_shr in peek_bits_triple field extraction\n\nPlain >> panics in debug mode when shift reaches 64 (e.g. n3+n2=64\nwith n1=0). Use wrapping_shr for consistency with peek_bits.\n\n* perf(bit_io): revert mask_lower_bits to debug_assert and suppress BMI2 dead_code\n\n- Revert assert! back to debug_assert! — callers guarantee n <= 56,\n  and the BIT_MASK table panics on OOB in release anyway. The assert!\n  added a branch to every peek_bits call on the hot decode path.\n- Add cfg_attr(allow(dead_code)) on BIT_MASK for BMI2 target builds\n  where the table is only referenced by tests.\n\n* fix(bit_io): document peek_bits_triple sum contract and lint benches in CI\n\n- Add doc comment specifying sum must equal n1+n2+n3\n- Add --benches flag to CI bench_internals clippy step so the\n  bitstream benchmark is actually compiled and linted\n\n* fix(bit_io): compute triple sum in u16 to prevent u8 overflow\n\nn1 + n2 + n3 in u8 can wrap in release builds (debug builds panic).\nUse u16 for addition in get_bits_triple and peek_bits_triple's\ndebug_assert_eq, cast to u8 only after confirming sum <= 56.",
          "timestamp": "2026-04-03T18:46:17+03:00",
          "tree_id": "093a095a01c5e8704c71195c65b85c54f93bcbf6",
          "url": "https://github.com/structured-world/structured-zstd/commit/aa92d4f9adebd0c723c73733644cb5ea3684c1a6"
        },
        "date": 1775232931817,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 4.263,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.153,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.092,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.249,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.214,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 5.701,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.559,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.103,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.674,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.444,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.455,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.134,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.083,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.228,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.302,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.63,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.709,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 110.871,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.735,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 59.705,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.354,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 61.108,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.239,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.752,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.277,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 172.999,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 66.72,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.61,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 56.714,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 0.909,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.302,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 8.787,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.235,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.882,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.034,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.224,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 21.843,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.177,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 90.107,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.811,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 77.634,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.238,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 77.646,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 12.643,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.408,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.637,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.636,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.967,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.015,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.514,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.97,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.052,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.505,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.642,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.812,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.006,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.481,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.631,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.752,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.982,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.556,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.919,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.699,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.569,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.791,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.721,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.707,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.794,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.721,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.657,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.796,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.792,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.144,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.666,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.102,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.181,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.337,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.444,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.714,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.377,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.273,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.205,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.286,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.799,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 37.084,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "98c1be0767d3892d54f51b7b83c2d01c0b197962",
          "message": "perf(decoding): pre-allocate decode buffer from sequence block analysis (#59)\n\n* perf(decoding): pre-allocate decode buffer from sequence block analysis\n\n- Block-level: calculate exact output size (sum of match lengths +\n  literals buffer length) before executing sequences, issue a single\n  reserve() call instead of per-sequence re-allocations\n- Frame-level: when frame_content_size is declared in the header,\n  pre-allocate the decode buffer upfront to avoid incremental growth\n- RLE/Raw blocks: reserve decompressed_size before the push loop\n\nEliminates repeated re-allocations in the hot decode path.\n\nCloses #20\n\n* fix(decoding): bound pre-allocation against malformed input\n\n- Enforce MAXIMUM_ALLOWED_WINDOW_SIZE in FrameDecoderState::new()\n- Clamp sequence pre-allocation to MAX_BLOCK_SIZE (128KB)\n- Document decode_block_content, FrameDecoderState::new/reset\n\n* perf(decoding): use MAX_BLOCK_SIZE for sequence pre-allocation\n\n- Replace per-block exact sum with constant MAX_BLOCK_SIZE reserve,\n  eliminating extra iteration over sequences and overflow risk\n- Fix WindowSizeTooBig error message to report the enforced\n  implementation limit (100 MiB) instead of the spec maximum\n- Make MAXIMUM_ALLOWED_WINDOW_SIZE pub(crate) with doc comment\n\n* perf(decoding): limit frame-level reserve to window_size\n\n- Remove FCS-based pre-allocation that could reserve up to 100 MiB\n  even for streaming callers that drain incrementally\n- Keep window_size reservation in new() for initial capacity\n- Consolidate duplicate doc comment on MAXIMUM_ALLOWED_WINDOW_SIZE\n\n* refactor(decoding): move MAXIMUM_ALLOWED_WINDOW_SIZE to common module\n\n- Relocate constant from frame_decoder to crate::common\n- Clarify decode_block_content and reset() doc strings\n- Reference shared constant from errors module\n\n* perf(decoding): inline DecodeBuffer::reserve forwarding method",
          "timestamp": "2026-04-03T21:30:14+03:00",
          "tree_id": "f452688833ef009d16c5999010a902ae6c4438b8",
          "url": "https://github.com/structured-world/structured-zstd/commit/98c1be0767d3892d54f51b7b83c2d01c0b197962"
        },
        "date": 1775242737239,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 4.311,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.147,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.086,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.243,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.378,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.212,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.017,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.022,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.554,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.096,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.622,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.425,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.611,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.124,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.076,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.203,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.303,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.647,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.726,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 106.035,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.817,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.997,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.333,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 66.517,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 19.446,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.461,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 175.306,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.333,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 66.849,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.657,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 56.907,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 0.92,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.352,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.144,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.692,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.235,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.781,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.628,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.59,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.193,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.291,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.282,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 89.111,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.931,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 77.791,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.292,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 77.807,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 12.62,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.343,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.543,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.959,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.81,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.503,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.823,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.436,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.623,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.701,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.989,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.41,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.616,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.612,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.302,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.675,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.951,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.753,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.655,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.668,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.821,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.8,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.92,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.829,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.83,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.658,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.915,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.847,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.19,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.664,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.103,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.181,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.333,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.449,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.715,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.472,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.314,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.39,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.309,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.814,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 37.056,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "cd31f5e8d64600c12a7701b0864d7196b59821a3",
          "message": "feat(encoding): write frame content size in encoder output (#60)\n\n* feat(encoding): write frame content size in encoder output\n\n- FrameCompressor now buffers compressed blocks and writes the frame\n  header after all input is consumed, so the total uncompressed size\n  is known and encoded as Frame_Content_Size (FCS)\n- StreamingEncoder gains set_pledged_content_size() for callers who\n  know the input size upfront; finish() verifies the pledge\n- Fix FCS descriptor flag mapping bug (8 => 3, was 3 => 8)\n- Add find_fcs_field_size() with correct +256 offset range for\n  2-byte FCS fields (256–65791 vs find_min_size's 256–65535)\n- Always include window descriptor alongside FCS (no single_segment)\n  to avoid C zstd incompatibility with compressed blocks whose\n  encoder window exceeds the declared content size\n\nCloses #16\n\n* style: apply cargo fmt\n\n* fix(encoding): correct FCS docstrings and add serialize_fcs preconditions\n\n- Fix find_fcs_field_size doc: only 1-byte encoding is gated on\n  single_segment; 2-byte range (256–65791) works regardless\n- Fix set_pledged_content_size doc: remove stale Single_Segment\n  claim since encoder always includes the window descriptor\n- Add debug_assert guards to serialize_fcs against underflow\n  when field_size==2 and val<256, reject unexpected field_size\n  values with unreachable!()\n\n* perf(encoding): prepend FCS header into block buffer to avoid extra copy\n\nSplice the small header (max 14 bytes) into the front of all_blocks\ninstead of writing header and blocks as two separate drain calls,\neliminating the second full-size buffer allocation.\n\n* fix(encoding): guard zero window_size and sticky error in pledged size API\n\n- Add assert for window_size == 0 in FrameCompressor::compress(),\n  matching the existing guard in StreamingEncoder\n- Call ensure_open() in set_pledged_content_size() to honor sticky\n  error state before accepting a pledge\n- Clarify README FCS line: auto in FrameCompressor, pledge-based\n  in StreamingEncoder\n\n* fix(encoding): enforce pledge bounds during write and validate before header\n\n- Move pledge validation in finish() before ensure_frame_started() to\n  avoid emitting a header with incorrect FCS on mismatch\n- Reject writes exceeding pledged content size in write() rather than\n  only detecting at finish()\n- Rename bytes_written → bytes_consumed to clarify it tracks\n  uncompressed input, not drain output\n- Revert splice prepend to two write_all calls — splice is O(n)\n  memmove vs two cheap sequential writes\n\n* docs(encoding): clarify pledge validation comment in finish()\n\nThe comment claimed validation happens \"before emitting the frame\nheader\", but the header is already written on the first write() via\nensure_frame_started(). Reword to accurately describe that validation\nhappens before finalizing the frame.\n\n* test(encoding): fix misleading test literal in pledge mismatch test\n\nReplace \"only 10 bytes\" (actually 13 bytes) with \"short payload\"\nand add a clarifying comment.\n\n* fix(encoding): use checked arithmetic for pledge enforcement and partial writes\n\n- Replace overflow-prone addition with checked_sub + truncation to\n  honor Write partial-write semantics (accept up to remaining pledge)\n- Add write_exceeding_pledge_returns_error test\n- Update compress() docstring to document O(compressed_size) buffering\n\n* fix(encoding): check pledge before header emission and strengthen FCS tests\n\n- Move pledge exhaustion check before ensure_frame_started() so that\n  misuse like pledge(0) + write(data) doesn't emit a partial header\n- Assert descriptor.frame_content_size_bytes() in FCS tests to\n  distinguish \"FCS present with value 0\" from \"FCS omitted\"\n\n* test(encoding): add 2-byte FCS case and partial-progress pledge write test\n\n- Add 300-byte input to FCS interop matrix to exercise the 256–65791\n  two-byte FCS encoding path with -256 offset\n- Add write_straddling_pledge_reports_partial_progress test to verify\n  write() returns Ok(n) at the pledge boundary instead of hard error\n\n* refactor(encoding): extract large test vec into named local binding",
          "timestamp": "2026-04-04T13:34:37+03:00",
          "tree_id": "311963a2d909985384c302400707941c3bdbdd8b",
          "url": "https://github.com/structured-world/structured-zstd/commit/cd31f5e8d64600c12a7701b0864d7196b59821a3"
        },
        "date": 1775300593074,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 4.089,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.108,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.291,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.431,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.212,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.274,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.534,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.116,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.671,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.381,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.268,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.151,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.253,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.275,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.532,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.694,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 94.159,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.704,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 55.331,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.111,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.132,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.701,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.406,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.247,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 158.716,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 66.677,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.563,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 57.001,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 0.982,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.375,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.149,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 8.558,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.222,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.951,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.588,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.068,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.133,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.019,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.671,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 87.184,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.262,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 78.404,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.883,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 78.766,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 10.733,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.337,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.613,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.607,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.96,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.814,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.503,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.88,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.423,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.629,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.729,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.991,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.4,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.617,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.651,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.966,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.117,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.299,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.307,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.555,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.919,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.699,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.466,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.793,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.698,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.669,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.791,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.693,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.655,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.601,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.793,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.043,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.665,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.103,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.181,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.332,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.429,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.738,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.384,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.283,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.09,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.068,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.408,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 36.75,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "bbd826e912f639251a944c70d438519aaa1413de",
          "message": "chore: release v0.0.6 (#57)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-04T14:40:13+03:00",
          "tree_id": "d5dbc729f4ee2ff2c4b5261848f7e192f9287e59",
          "url": "https://github.com/structured-world/structured-zstd/commit/bbd826e912f639251a944c70d438519aaa1413de"
        },
        "date": 1775304523271,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 4.409,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.019,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.181,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.107,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.401,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.212,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 5.932,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.033,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.673,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.131,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.753,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.391,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.328,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.152,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.097,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.254,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.359,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.574,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.691,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 97.635,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.756,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 59.572,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.143,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 64.198,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.705,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.329,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.234,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 163.574,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.294,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 74.149,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.585,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 64.446,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 0.906,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.369,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 8.754,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.221,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.901,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.583,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.028,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.13,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.579,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.649,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 92.164,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.324,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 89.076,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.657,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 77.533,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 10.602,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.366,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.603,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.963,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.813,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.903,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.033,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.426,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.733,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.993,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.441,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.658,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.964,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.175,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.307,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.307,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.699,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.916,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.846,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.653,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.606,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.788,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.835,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.652,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.689,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.789,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.833,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.653,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.67,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.791,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.047,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.667,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.101,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.18,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.332,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.436,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.748,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.554,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.334,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.139,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.108,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.498,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 36.831,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "d8260fad35665221ce285af126d923e2dc067dcc",
          "message": "feat(encoding): numeric compression levels (1-22) API (#63)\n\n* feat(encoding): add numeric compression levels api\n\n- add CompressionLevel::Level(i32) and canonical from_level mapping\n\n- port level table, source-size hinting, and matcher sizing behavior\n\n- align CLI parsing/range validation and update docs/tests\n\n* fix(encoding): clamp source-size hint to dictionary history\n\n* fix(encoding): avoid shrinking pooled matcher buffers\n\n* docs(readme): clarify numeric-level compatibility scope\n\n* fix(encoding): tighten level hint behavior and docs\n\n* fix(encoding): account payload with dictionary size hints\n\n* fix(encoding): clarify and verify dictionary size hints\n\n* test(encoding): add decode roundtrip for dictionary hint floor\n\n* fix(encoding): align size-hint handling and regressions\n\n- use source_size_hint in CLI instead of hard pledged size from metadata\n\n- rename dfast hash helper to hash_index for clearer field/method separation\n\n- pin late set_source_size_hint InvalidInput behavior after write\n\n* fix(encoding): tighten source-hint semantics and tests\n\n- keep dictionary source-size hint scoped to payload bytes in FrameCompressor\n\n- relax dictionary window assertions to roundtrip + nonincreasing-window guarantees\n\n- pin dfast hinted table size and document visible set_source_size_hint effects\n\n* docs(encoding): align source size hint contract\n\n- update FrameCompressor::set_source_size_hint docs to payload-only semantics\n\n- document that dictionary priming does not inflate advertised window",
          "timestamp": "2026-04-05T11:45:00+03:00",
          "tree_id": "f1de7af9e1add3b7f990012ae375ee120c6b399a",
          "url": "https://github.com/structured-world/structured-zstd/commit/d8260fad35665221ce285af126d923e2dc067dcc"
        },
        "date": 1775380413233,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 6.592,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.177,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.112,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.294,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.421,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.213,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.95,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.605,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.12,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.687,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.39,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 7.113,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.151,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.099,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.257,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.366,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.385,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.794,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 145.967,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.845,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.606,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.787,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 99.583,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 26.387,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 24.999,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.266,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 252.759,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 93.938,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.702,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 145.877,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 2.116,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.419,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.147,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 13.614,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.254,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.264,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.68,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.788,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.884,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 23.545,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.495,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 120.168,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 4.03,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 69.891,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.977,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 70.557,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 13.849,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.409,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.632,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.761,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.985,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.831,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.508,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.983,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.066,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.507,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.643,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.826,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.025,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.45,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.765,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.995,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.119,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.028,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.19,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.123,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.186,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.036,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.122,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.036,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.186,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.121,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.186,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.036,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.277,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.19,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.313,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.321,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.243,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.322,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.83,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 2.01,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.045,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.668,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 3.012,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.866,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.066,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.674,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 3.468,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.924,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.673,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 3.422,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.896,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.506,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.68,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.103,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.338,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.014,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.555,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.865,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.668,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.73,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 14.108,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 18.101,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 22.404,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 39.262,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "d205d6605df7d14f1718e7bc18fab4d11dc4f71b",
          "message": "perf(bench): add rust/ffi delta benchmark artifacts (#65)\n\n* perf(bench): add rust/ffi delta benchmark artifacts\n\n* fix(bench): align delta artifacts and parity docs\n\n* perf(bench): drop threshold coefficients from rust/ffi delta\n\n- Remove threshold/status classification from delta artifacts\n\n- Keep direct same-run Rust vs FFI delta values only\n\n- Update benchmark docs to reflect raw relative comparison\n\n* perf(bench): restore parity status without env coefficients\n\n- Reintroduce faster/slower/parity labels from raw same-run deltas\n\n- Keep rust/ffi delta values as source of truth\n\n- Document that no calibration/pre-test coefficients are applied\n\n* fix(bench): address benchmark delta review feedback\n\n- Fail fast on unsupported benchmark name shapes\n\n- Preserve ratio metadata in delta row param fallback\n\n- Use explicit None checks for speed delta math\n\n- Clarify interpretation strings as delta semantics\n\n- Upload benchmark-delta artifacts in CI benchmark job\n\n* fix(bench): render parity reference band in delta markdown\n\n- Add explicit 0.99–1.05 guide text to ratio delta section\n\n- Add explicit 0.99–1.05 guide text to speed delta section\n\n* chore(bench): export delta reference band to json",
          "timestamp": "2026-04-05T19:15:26+03:00",
          "tree_id": "2bfdf866007c04f382321820e0711312059cc462",
          "url": "https://github.com/structured-world/structured-zstd/commit/d205d6605df7d14f1718e7bc18fab4d11dc4f71b"
        },
        "date": 1775407437340,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 6.292,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.019,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.108,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.293,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.426,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.208,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 7.759,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.578,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.112,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.687,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.833,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.151,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.256,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.362,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.695,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.682,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 102.307,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.744,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 55.789,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.214,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 62.375,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.162,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.265,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.225,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 165.79,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.295,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 65.769,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.575,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 57.608,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 1.003,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.301,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.19,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.236,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.795,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.633,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.066,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.204,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 21.693,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.147,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 97.956,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.816,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 78.653,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.307,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 78.533,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 12.586,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.327,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.612,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.596,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.961,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.798,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.503,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.881,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.413,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.751,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.992,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.389,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.617,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.672,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.968,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.176,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.036,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.311,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.298,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.578,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.922,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.864,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.638,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.799,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.872,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.658,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.759,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.822,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.748,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.658,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.927,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.816,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.021,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.668,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.102,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.18,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.332,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.452,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.718,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.515,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.361,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.226,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.236,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.651,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 37.168,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b280757e033d5771d9c9cdc0f70e887b0d15c2e3",
          "message": "chore: release v0.0.7 (#64)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-06T00:01:44+03:00",
          "tree_id": "ca98c72dc1a9af6ad700f8b81821b5bbeeb47f8c",
          "url": "https://github.com/structured-world/structured-zstd/commit/b280757e033d5771d9c9cdc0f70e887b0d15c2e3"
        },
        "date": 1775424620794,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 4.653,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.092,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.249,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.557,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.208,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.253,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.605,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.129,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.739,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.414,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.748,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.124,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.077,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.203,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.28,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.62,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.724,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 104.689,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.7,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 55.778,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.318,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 62.215,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.017,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.372,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.251,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 175.99,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.324,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 66.974,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.636,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 57.27,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 1.02,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.344,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.142,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.565,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.249,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.007,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.667,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.175,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.321,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.428,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.149,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 109.067,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.902,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 78.176,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.424,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 78.488,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 12.903,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.327,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.613,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.612,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.962,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.8,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.503,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.856,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.033,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.416,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.626,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.724,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.992,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.389,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.616,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.639,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.968,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.036,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.12,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.038,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.301,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.313,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.313,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.321,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.311,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.713,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.923,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.743,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.655,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.758,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.8,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.777,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.653,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.843,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.811,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.768,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.655,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.776,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.8,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.134,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.679,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.015,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.102,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_without_dict",
            "value": 0.18,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-10k-random/matrix/c_ffi_with_dict",
            "value": 0.332,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_without_dict",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/small-4k-log-lines/matrix/c_ffi_with_dict",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 2.442,
            "unit": "ms"
          },
          {
            "name": "compress-dict/fastest/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 2.722,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 5.373,
            "unit": "ms"
          },
          {
            "name": "compress-dict/default/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 6.273,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 13.256,
            "unit": "ms"
          },
          {
            "name": "compress-dict/better/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 17.243,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_without_dict",
            "value": 19.851,
            "unit": "ms"
          },
          {
            "name": "compress-dict/best/decodecorpus-z000033/matrix/c_ffi_with_dict",
            "value": 37.208,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "71708e554ae67a8cea8bac3554ab2a3e1c0621b7",
          "message": "fix(ci): publish benchmark delta reports (#75)\n\n* fix(ci): publish benchmark delta reports\n\n- make run-benchmarks tolerate missing REPORT_DICT_TRAIN rows\\n- keep benchmark report sections explicit with n/a placeholders\\n- publish benchmark-report and benchmark-delta artifacts to gh-pages/dev/bench\\n- add direct README links to published delta/full reports\n\n* fix(review): align benchmark warning and publish label\n\n- update REPORT_DICT_TRAIN warning to mention _n/a_ placeholder output\n- rename gh-pages publish commit message to benchmark reports\n\n* fix(review): align report-dict warning with placeholder\n\n- update REPORT_DICT warning to mention _n/a_ placeholder rows when section has no data",
          "timestamp": "2026-04-06T18:48:11+03:00",
          "tree_id": "e06e8e921a6c6cb57f4f22e1fea5760bdc5cac65",
          "url": "https://github.com/structured-world/structured-zstd/commit/71708e554ae67a8cea8bac3554ab2a3e1c0621b7"
        },
        "date": 1775491978620,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.024,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.387,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.092,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.256,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.34,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.208,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.839,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.603,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.096,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.64,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.293,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.969,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.125,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.076,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.203,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.274,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.568,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.693,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 104.595,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.671,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.947,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.198,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 61.678,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.563,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.53,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.234,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 163.43,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 68.919,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.566,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 59.394,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 0.912,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.336,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.18,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.33,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.23,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.14,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.558,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.222,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.077,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.172,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.57,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 98.413,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.192,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 78.701,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.106,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 80.599,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 14.975,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.247,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.491,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.967,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.749,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.728,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.035,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.326,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.626,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.579,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.993,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.303,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.51,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.967,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.174,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.307,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.301,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.311,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.796,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.994,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.065,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.663,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.886,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.794,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.761,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.652,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.846,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.799,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.747,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.851,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.804,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.047,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.663,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "baf2ffd017a49f26c8114ef42a82cf6d4058ff7c",
          "message": "perf(fse): pack decoder entries and align decode tables (#76)\n\n* perf(fse): pack decoder entry to 4-byte layout\n\n- replace Entry.base_line(u32) with Entry.new_state(u16)\n- keep decode transition semantics (new_state + low bits)\n- update FSE/sequence tests and add size assertion for packed entry\n\n* perf(fse): align decode tables and bulk spread symbols\n\n* fix(fse): validate acc log bounds and reuse spread buffer\n\n* fix(fse): address review feedback for packed decode path\n\n- Clamp build_decoder max_log to entry layout limit instead of early reject\n\n- Add explicit layout assertions and tighten unsafe write safety invariants\n\n- Update regression test to validate decoder path behavior\n\n* test(fse): cover clamp path and enforce entry layout\n\n- exercise build_decoder clamp branch with max_log > 16\n\n- add compile-time size and field-offset assertions for Entry on little-endian\n\n* perf(fse): preserve spread-buffer reuse on table reinit\n\n- copy symbol_spread_buffer in reinit_from to retain allocated capacity\n\n* perf(fse): avoid spread-buffer copy on table reinit\n\n- preserve only symbol_spread_buffer capacity via reserve\n\n- rename empty-input test to match asserted behavior\n\n* refactor(fse): remove stale debug print in update_state\n\n- drop commented println that logged post-update state and could mislead debugging",
          "timestamp": "2026-04-07T03:12:58+03:00",
          "tree_id": "51ef922c82d42198f0d92b8a7acc3ca29ea60aed",
          "url": "https://github.com/structured-world/structured-zstd/commit/baf2ffd017a49f26c8114ef42a82cf6d4058ff7c"
        },
        "date": 1775522255463,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-1k-random/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-1k-random/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/pure_rust",
            "value": 5.373,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-1k-random/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-1k-random/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/pure_rust",
            "value": 0.301,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-1k-random/matrix/c_ffi",
            "value": 0.404,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/pure_rust",
            "value": 0.208,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-10k-random/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/pure_rust",
            "value": 6.705,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-10k-random/matrix/c_ffi",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/pure_rust",
            "value": 0.665,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-10k-random/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/pure_rust",
            "value": 0.77,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-10k-random/matrix/c_ffi",
            "value": 0.394,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.778,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.151,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.257,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.373,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.764,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.695,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 103.576,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.755,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 59.279,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.218,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 72.848,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.23,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/pure_rust",
            "value": 23.564,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/high-entropy-1m/matrix/c_ffi",
            "value": 0.267,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/pure_rust",
            "value": 174.401,
            "unit": "ms"
          },
          {
            "name": "compress/default/high-entropy-1m/matrix/c_ffi",
            "value": 0.343,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/pure_rust",
            "value": 73.743,
            "unit": "ms"
          },
          {
            "name": "compress/better/high-entropy-1m/matrix/c_ffi",
            "value": 0.664,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/pure_rust",
            "value": 65.699,
            "unit": "ms"
          },
          {
            "name": "compress/best/high-entropy-1m/matrix/c_ffi",
            "value": 1.005,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.346,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.158,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.604,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.222,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.305,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.587,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.942,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.19,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/pure_rust",
            "value": 22.428,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/large-log-stream/matrix/c_ffi",
            "value": 2.67,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/pure_rust",
            "value": 101.681,
            "unit": "ms"
          },
          {
            "name": "compress/default/large-log-stream/matrix/c_ffi",
            "value": 3.336,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/pure_rust",
            "value": 81.634,
            "unit": "ms"
          },
          {
            "name": "compress/better/large-log-stream/matrix/c_ffi",
            "value": 10.757,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/pure_rust",
            "value": 71.097,
            "unit": "ms"
          },
          {
            "name": "compress/best/large-log-stream/matrix/c_ffi",
            "value": 10.802,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/pure_rust",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-1k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-10k-random/c_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.226,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.608,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.59,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.96,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.739,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.5,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.765,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.031,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.325,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.624,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.617,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.993,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.292,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.543,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.968,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.184,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.182,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/default/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.155,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/better/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.118,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "decompress/best/high-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.316,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.327,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.238,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.327,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.316,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.32,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.238,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.328,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.734,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.946,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.893,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.694,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.825,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.904,
            "unit": "ms"
          },
          {
            "name": "decompress/default/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.654,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 3.018,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.833,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/pure_rust",
            "value": 2.889,
            "unit": "ms"
          },
          {
            "name": "decompress/better/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.656,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/pure_rust",
            "value": 2.816,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/rust_stream/matrix/c_ffi",
            "value": 1.823,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/pure_rust",
            "value": 3.169,
            "unit": "ms"
          },
          {
            "name": "decompress/best/large-log-stream/c_stream/matrix/c_ffi",
            "value": 0.666,
            "unit": "ms"
          }
        ]
      }
    ],
    "structured-zstd vs C FFI (x86_64-gnu)": [
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "6ee8b0952421b012f4afb4412cd2c36f96f3cb22",
          "message": "perf(bench): multi-arch relative Rust-vs-FFI dashboard (#78)\n\n* perf(bench): publish multi-target relative dashboard\n\n- add target-aware benchmark generation and benchmark-relative.json\n- run CI benchmark matrix for x86_64-gnu, i686-gnu, x86_64-musl\n- publish relative-first gh-pages dashboard with filters and merged payloads\n- document relative metrics and matrix coverage in benchmark docs\n\nCloses #77\n\n* fix(bench): align target ids and preserve report payloads\n\n- use stable short target id in benchmark artifacts and metadata\n- merge markdown report bodies per target instead of replacing with counts-only summary\n- add target filter and parity-band rendering in dashboard\n- load reference band from payload and remove JSON-driven innerHTML rendering\n\n* fix(bench): preserve relative series and reference bands\n\n- make dashboard parity-band copy data-driven from payload\n- group chart/status by target+benchmark key to avoid row overwrite\n- preserve reference_band when merging benchmark-relative payloads\n\n* ci(bench): parallelize matrix benchmark jobs\n\n* ci(bench): align alerts with persisted baseline\n\n* fix(bench-ui): fallback default metric selection\n\n* ci(bench): trim report headings and retain recent payload\n\n* ci(workflow): keep main pushes from mid-flight cancellation\n\n* ci(workflow): gate benchmark action to canonical target\n\n* ci(bench): relax noisy regression checks\n\n* ci(bench): validate merge path and preserve target metadata\n\n* ci(bench): keep benchmark series names stable\n\n* ci(bench): extract benchmark merge script\n\n* fix(bench-dashboard): stabilize snapshot ordering\n\n* fix(ci): harden benchmark merge and gating\n\n* chore(review): address final nitpick threads\n\n* fix(bench): harden smoke-set result handling\n\n* fix(bench): tighten merge artifact validation",
          "timestamp": "2026-04-07T22:49:06+03:00",
          "tree_id": "bac9a2b55f46538e73dd24c098c2757d9d27f5af",
          "url": "https://github.com/structured-world/structured-zstd/commit/6ee8b0952421b012f4afb4412cd2c36f96f3cb22"
        },
        "date": 1775592875663,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.608,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.022,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.151,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 112.626,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.677,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.409,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.196,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.896,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.21,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.598,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.557,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.75,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.499,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.763,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.03,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.333,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.622,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.65,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.988,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.307,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.313,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b6f9a01f337bd6d68af7da6a72245dc0d62872d4",
          "message": "chore: release v0.0.8 (#74)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-07T23:21:58+03:00",
          "tree_id": "0ed453f8b5e6e4b290fbe003c45e7a5bab8cb6d0",
          "url": "https://github.com/structured-world/structured-zstd/commit/b6f9a01f337bd6d68af7da6a72245dc0d62872d4"
        },
        "date": 1775595058833,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.961,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.088,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 120.276,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.333,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.875,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.564,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 11.189,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.232,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.117,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.624,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.888,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.453,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.056,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.996,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.522,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.621,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.914,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.957,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.313,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.322,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.323,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "a403575374518968627943a09f9f48705828427c",
          "message": "fix(bench): compact dashboard status block (#79)",
          "timestamp": "2026-04-07T23:42:23+03:00",
          "tree_id": "8ce0d4c3e0608fc351b2f011ce2918e93551c9b8",
          "url": "https://github.com/structured-world/structured-zstd/commit/a403575374518968627943a09f9f48705828427c"
        },
        "date": 1775597113100,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.869,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.123,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.097,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 94.752,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.695,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 55.864,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.22,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.013,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.221,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.029,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.583,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.76,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.5,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.776,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.07,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.345,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.628,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.651,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.328,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.318,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "aad0d80ed47865617ac024001e8c401f969531a8",
          "message": "perf(encoding): reuse streaming encoded scratch buffer (#80)\n\n* perf(encoding): reuse streaming encoded scratch buffer\n\n- add encoded_scratch buffer to StreamingEncoder and reuse it per block\n\n- remove per-block Vec allocation churn on encode_block hot path\n\n- add test proving encoded scratch capacity reuse across block emits\n\nRefs #47\n\n* fix(encoding): reserve streaming scratch by required len\n\n- use reserve(needed_capacity.saturating_sub(encoded.len()))\n\n- avoid incorrect reserve calculation based on current capacity",
          "timestamp": "2026-04-08T11:31:29+03:00",
          "tree_id": "097c1cae5ada96f98ecdb5bfd5806a5a877ae2ff",
          "url": "https://github.com/structured-world/structured-zstd/commit/aad0d80ed47865617ac024001e8c401f969531a8"
        },
        "date": 1775638580718,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.874,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.134,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.087,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 133.92,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.345,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 77.96,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.856,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 12.796,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.23,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 3.815,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.626,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.886,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.468,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.079,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.533,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.621,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.971,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.969,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.325,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.327,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "1f379be5fdcc92d3cc04b15d22721f6949eead9e",
          "message": "chore: release v0.0.9 (#81)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-08T12:09:16+03:00",
          "tree_id": "c42eead604b144d16eeeda7aec3ac682061f9b0e",
          "url": "https://github.com/structured-world/structured-zstd/commit/1f379be5fdcc92d3cc04b15d22721f6949eead9e"
        },
        "date": 1775640854676,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 5.027,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.125,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.077,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 104.909,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.698,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 58.469,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.172,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.548,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.21,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.003,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.556,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.753,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.502,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.763,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.031,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.329,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.622,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.647,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.99,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.303,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.316,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "c9ab3d318bbb55cda880eb35259018faeae54567",
          "message": "perf(encoding): rebase hc positions past u32 boundary (#82)\n\n* perf(encoding): rebase hc positions past u32 boundary\n\n- store HC table indexes relative to a moving position base\n\n- rebase and rebuild live chain/hash entries before u32 overflow\n\n- add regression test for post-u32 boundary insertion path\n\n- remove obsolete 4 GiB limitation notes for HC levels\n\nCloses #51\n\n* test(encoding): avoid 32-bit overflow in hc rebase regression\n\n- make hc_rebases_positions_after_u32_boundary platform-safe\n\n- keep 64-bit >u32 scenario, use boundary path on 32-bit\n\n- prevent const arithmetic overflow on i686 CI\n\n* test(encoding): make hc u32-boundary regression 32-bit safe\n\n- avoid const overflow in i686 by using fallible conversion\n\n- clarify relative-position and chain-candidate storage comments\n\n* test(encoding): verify hc chain candidates after rebase\n\n- extend u32-boundary regression to assert chain_candidates returns valid entries\n\n- addresses CodeRabbit nitpick on post-rebase matchability\n\n* fix(encoding): rebuild hc rebase tables only for seen prefix",
          "timestamp": "2026-04-08T17:35:00+03:00",
          "tree_id": "248559bd9577608be29a89dabe6bc390e97ed984",
          "url": "https://github.com/structured-world/structured-zstd/commit/c9ab3d318bbb55cda880eb35259018faeae54567"
        },
        "date": 1775660385608,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.994,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.153,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.077,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 101.326,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.696,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.608,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.236,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.552,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.23,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.767,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.587,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.743,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.502,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.708,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.031,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.332,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.612,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.991,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.3,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.238,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.32,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "9f905ade7fadc7b22866104eef768c9dbe4df69b",
          "message": "perf(encoding): add row-based match finder backend (#84)\n\n* perf(encoding): add row-based match finder backend\n\n* test(encoding): raise row matcher coverage and decouple min match const\n\n* fix(encoding): stabilize row hash key across block boundaries\n\n* test(encoding): cover row dictionary retention paths\n\n* test(encoding): close row matcher patch coverage gaps\n\n* fix(encoding): harden row backend reset and hash extraction\n\n* perf(bench): add level4 row lane to regression matrix\n\n* docs(encoding): document new row matcher checks\n\n* test(encoding): make short-block row assertion effective\n\n* refactor(encoding): share row and dfast heuristics\n\n* ci(bench): raise i686 benchmark timeout\n\n* ci(bench): derive regression levels from bench output",
          "timestamp": "2026-04-08T22:13:17+03:00",
          "tree_id": "a66931eed7cefc9e8e431994e4edaa4b798e0f2b",
          "url": "https://github.com/structured-world/structured-zstd/commit/9f905ade7fadc7b22866104eef768c9dbe4df69b"
        },
        "date": 1775677460570,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.689,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.146,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.09,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.214,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.04,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.247,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.32,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.797,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.706,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 96.201,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.701,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.652,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.077,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 45.036,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.898,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.93,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.813,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.369,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.158,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.232,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.222,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.867,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.586,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.601,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.242,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.936,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.042,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.228,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.532,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.96,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.737,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.74,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.028,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.318,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.624,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.647,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.988,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.793,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.511,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.771,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.312,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.613,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.531,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.964,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.314,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.306,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.306,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e877c448dfc573de789f09ccadf9f3866a08bda7",
          "message": "chore: release v0.0.10 (#83)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-08T23:10:49+03:00",
          "tree_id": "5ee37753a0a6f647ffae73b0a507917c9c3bf363",
          "url": "https://github.com/structured-world/structured-zstd/commit/e877c448dfc573de789f09ccadf9f3866a08bda7"
        },
        "date": 1775680923257,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.606,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.161,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.097,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.219,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.043,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.269,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.785,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.726,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 97.534,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.843,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.33,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.065,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 44.778,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.055,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.807,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.576,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.364,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.332,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.221,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.924,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.584,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.88,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.242,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.243,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.217,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.23,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.527,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.964,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.739,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.502,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.77,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.031,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.315,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.631,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.993,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.794,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.511,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.805,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.035,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.292,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.616,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.557,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.967,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.312,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.329,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.311,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.328,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.31,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.328,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.315,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.328,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.328,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "1112571acbf8a5809d6da5eb00e91d62bce1be59",
          "message": "perf(decoding): add runtime-dispatched simd wildcopy (#85)\n\n* perf(decoding): add runtime-dispatched simd wildcopy\n\n* fix(decoding): use std x86 feature detection in simd copy\n\n* refactor(decoding): cache simd wildcopy dispatch\n\n* refactor(decoding): tighten wildcopy contracts and tests\n\n* fix(decoding): close lint and review nitpicks\n\n* refactor(decoding): pass real capacities in simd helper\n\n* perf(decoding): keep x86 simd strategy in no_std builds\n\n* test(decoding): cover misaligned wildcopy overshoot path\n\n* test(decoding): add simd copy path unit coverage\n\n* test(decoding): guard x86 simd tests behind std\n\n* fix(decoding): tighten wildcopy safety contract and sse2 gating\n\n* perf(decoding): thread branchless wildcopy capacities",
          "timestamp": "2026-04-09T09:51:58+03:00",
          "tree_id": "2ff3584855ab55fe848173bbd8141edcb0ec5ca8",
          "url": "https://github.com/structured-world/structured-zstd/commit/1112571acbf8a5809d6da5eb00e91d62bce1be59"
        },
        "date": 1775719365510,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.563,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.152,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.222,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.045,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.256,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.4,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.29,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.731,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 109.088,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.759,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.301,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.341,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 47.553,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.001,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 77.465,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 19.207,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.374,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.158,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.287,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.223,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.801,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.589,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.82,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.243,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.112,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.223,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.373,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.612,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.643,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.962,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.849,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.503,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.944,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.463,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.622,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.816,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.989,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.918,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.511,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.958,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.035,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.43,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.667,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.34,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.35,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.341,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.352,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.34,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.352,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.339,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.35,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.34,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.351,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "1b396b26710c25981190df0f30726f22a0eb0d4a",
          "message": "chore: release v0.0.11 (#89)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-09T10:35:43+03:00",
          "tree_id": "b03048c288574c9eaa1e6cc8ed4b1da4e2fdf22d",
          "url": "https://github.com/structured-world/structured-zstd/commit/1b396b26710c25981190df0f30726f22a0eb0d4a"
        },
        "date": 1775722085845,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.594,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.152,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.223,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.043,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.261,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.386,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.907,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.671,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 103.411,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.738,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 58.411,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.13,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 49.603,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.912,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 64.284,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.116,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.388,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.866,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.222,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.94,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.591,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.902,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.243,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.112,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.159,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.413,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.61,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.628,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.839,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.888,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.456,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.623,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.741,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.992,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.903,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.51,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.932,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.038,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.437,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.678,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.967,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.341,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.352,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e101e88627947735e457aa3ed3383a72a0247cc3",
          "message": "perf(decoding): branchless offset history, prefetch pipeline, and BMI2 triple extract (#90)\n\n* perf(decoding): optimize offset history, prefetch, and bit triple extraction\n\n- make sequence offset-history updates table-driven with targeted regression tests\n- add N+2 literal prefetch and multi-line stride prefetch with L1/L2 hints\n- route match-source prefetch through T1 with size gating\n- add runtime-dispatched BMI2 pext path for triple bit extraction with AMD Zen1/2 fallback\n\nRefs #69\n\n* fix(decoding): align prefetch cfg and offset guards\n\n- remove unnecessary unsafe wrappers around __cpuid dispatch checks\n- switch x86/x86_64 prefetch helpers to const-generic prefetch hints and fix fallback cfg gating\n- preserve ZeroOffset error path in branchless offset history and add regression test\n- sync PR/issue wording with current BMI2 3x pext implementation\n\n* perf(decoding): harden prefetch and branchless paths\n\n- make aarch64 prefetch hint compile-time via const generics and use readonly asm options\n- make N+2 literal prefetch range computation overflow-safe on usize targets\n- remove lit_len branch from do_offset_history via a single indexed rule table\n- add BMI2 triple-extract regression coverage against scalar reference\n\n* test(bit-reader): cover pext policy with pure helper\n\n- extract should_use_pext(vendor, family) from cpuid dispatch path\n- add table-driven tests for AMD Zen1/2 guard and non-AMD behavior\n- keep runtime dispatch semantics unchanged while improving branch coverage",
          "timestamp": "2026-04-09T14:34:41+03:00",
          "tree_id": "b75f3d61509d2f67d9c62bb349630b15c392b288",
          "url": "https://github.com/structured-world/structured-zstd/commit/e101e88627947735e457aa3ed3383a72a0247cc3"
        },
        "date": 1775736350699,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.481,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.154,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.078,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.182,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.203,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.281,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.961,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.701,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 110.239,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.758,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 58.555,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.181,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 46.626,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.971,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 66.406,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.887,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.387,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.149,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.675,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.226,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.105,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.596,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 9.478,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.249,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.075,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.221,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.648,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.612,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.839,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.96,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.051,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.502,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.212,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.74,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.043,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.993,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.108,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.512,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.19,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.035,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.696,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.923,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.276,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.199,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.381,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.334,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.334,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.346,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.334,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.241,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "209cebb081db29a3db5ae404483d73be63ec0dd4",
          "message": "chore: release v0.0.12 (#91)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-09T16:11:33+03:00",
          "tree_id": "9c31eb5c7b5738724196356a10913baad59d6ddf",
          "url": "https://github.com/structured-world/structured-zstd/commit/209cebb081db29a3db5ae404483d73be63ec0dd4"
        },
        "date": 1775742163202,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.492,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.139,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.087,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.191,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.038,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.23,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.827,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.866,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 132.706,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.407,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 93.583,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.599,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.485,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.88,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 100.962,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 21.1,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.382,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.166,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.762,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.231,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.693,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.624,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 5.504,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.251,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.8,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.234,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.793,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.601,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.073,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.901,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.162,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.449,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.479,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.914,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.621,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.323,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.962,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.472,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.502,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.006,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.873,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.606,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.217,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.931,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.352,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.261,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.363,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.363,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.363,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.363,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "786b00b57d2ed0ab7e57fb8aa16ca0ef33cda553",
          "message": "perf(decoding): SIMD HUF kernels with runtime dispatch (#92)\n\n* perf(decoding): fuse huf decode+advance and add kernel dispatch\n\n* perf(decoding): add avx2 gather fast path for huf 4-stream\n\n* perf(decoding): use bmi2 bzhi for huf state masking\n\n* perf(decoding): add neon 4-way unpack fast path for huf\n\n* perf(decoding): add vbmi2 and sve huf kernel dispatch\n\n* perf(decoding): implement sve asm unpack kernel for huf\n\n* fix(decoding): address copilot and coderabbit huf review feedback\n\n* fix(decoding): remove redundant unsafe around vbmi2 compress\n\n* fix(huff0): gate vbmi2 with avx512f and assert decoder invariants\n\n* test(huff0): increase decoder coverage and fix asm safety\n\n* fix(huff0): remove redundant unsafe in x86_64 bmi2 path\n\n* test(huff0): add vbmi2 parity and release fallback coverage\n\n* test(huff0): add neon and sve decode parity checks\n\n* test(huff0): handle debug asserts in mixed fallback tests\n\n* fix(huff0): add fuzz-export shims for decoder steps\n\n* perf(huff0): remove simd spills and fix decode throughput metric\n\n* fix(huff0): address remaining review lint paths\n\n* fix(huff0): enable vbmi2 test path and clean unsafe\n\n* test(huff0): guard vbmi2 parity test by cpu features\n\n* fix(huff0): gate std runtime tests and hoist decode4 checks\n\n* fix(huff0): wrap intrinsic calls in explicit unsafe blocks\n\n* fix(huff0): drop redundant unsafe wrappers for intrinsics\n\n* perf(huff0): tighten decode4 contracts and scalar fast path\n\n* docs(huff0): align decode docs and loop comments with runtime path\n\n* perf(huff0): tighten asm flags and bench buffer reuse\n\n* fix(huff0): remove invalid preserves_flags from sve asm\n\n* perf(bench): reduce buffer overhead and dedup decode loop\n\n* perf(bench): black-box literals-heavy decode\n\n* docs(huff0): clarify unchecked decode cpu precondition\n\n* refactor(huff0): drop unused aarch64 target_feature attrs\n\n* fix(decoding): enforce exact single-stream huf termination",
          "timestamp": "2026-04-09T23:12:56+03:00",
          "tree_id": "41d4b5b959ace16e8a81ff8e91626da07ef7aa18",
          "url": "https://github.com/structured-world/structured-zstd/commit/786b00b57d2ed0ab7e57fb8aa16ca0ef33cda553"
        },
        "date": 1775767434009,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.381,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.136,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.086,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.19,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.039,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.225,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.309,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.557,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.834,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 126.322,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.341,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.598,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 50.735,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.898,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 66.955,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.613,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.425,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.805,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.254,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.701,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.628,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 5.525,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.273,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.806,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.206,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.791,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.603,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.328,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.9,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.163,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.429,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.259,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.996,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.89,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.985,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.954,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.238,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.484,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.258,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.864,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.608,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.924,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.927,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.355,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.372,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "ceb9e27fb9a2bb15c355e7069f71a54304835eec",
          "message": "chore: release v0.0.13 (#93)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-09T23:57:01+03:00",
          "tree_id": "619472f616a32f84bb8d6d48664397fd1253e6b6",
          "url": "https://github.com/structured-world/structured-zstd/commit/ceb9e27fb9a2bb15c355e7069f71a54304835eec"
        },
        "date": 1775770076006,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 4.756,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.15,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.076,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.183,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.255,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.983,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.698,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 105.391,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.666,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.885,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.309,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 45.422,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.841,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 64.777,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.097,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.328,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.179,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.712,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.242,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.97,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.585,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.748,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.262,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.049,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.153,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.622,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.712,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.963,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.04,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.501,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.72,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.034,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.718,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.504,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.994,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.103,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.51,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.72,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.035,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.697,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.405,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.342,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.355,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.334,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.343,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.238,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.356,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "544abd56fcd2a7e2fb1e2fd7b67dd4b9da085a46",
          "message": "perf(encoding): enable one-shot size hint across levels safely (#94)\n\n* perf(encoding): scope one-shot size hint to dfast default\n\n* perf(encoding): enable one-shot size hint across levels safely\n\n* fix(encoding): align hint docs and fixture coverage\n\n* test(encoding): improve hint regression diagnostics",
          "timestamp": "2026-04-10T00:58:35+03:00",
          "tree_id": "bbbe35f1079aa0f38da86dcd6388974cb5470ca2",
          "url": "https://github.com/structured-world/structured-zstd/commit/544abd56fcd2a7e2fb1e2fd7b67dd4b9da085a46"
        },
        "date": 1775773773373,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.067,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.05,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.077,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.058,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.05,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.277,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.935,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.719,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 105.211,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.707,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 57.734,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.172,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 47.686,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.976,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 70.642,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.769,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.362,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.148,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 12.276,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.222,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.135,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.591,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.777,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.244,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.309,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.185,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.642,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.73,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.958,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.047,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.5,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.778,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.03,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.739,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.623,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.535,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.989,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.118,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.51,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.746,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.032,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.711,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.613,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.49,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.963,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.331,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.351,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.333,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.327,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.327,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.334,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.358,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b0a3e17dc587547eaf2ea9194e449332f27a6005",
          "message": "chore: release v0.0.14 (#95)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-10T01:01:56+03:00",
          "tree_id": "ca8c9d2dd3b079802d5d456f821b5e22951892a2",
          "url": "https://github.com/structured-world/structured-zstd/commit/b0a3e17dc587547eaf2ea9194e449332f27a6005"
        },
        "date": 1775776484884,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.062,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.018,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.05,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.076,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.058,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.05,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.274,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 16.802,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.707,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 103.236,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.702,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 56.435,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.125,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 44.977,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.868,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 60.879,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.695,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.354,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.148,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 10.111,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.209,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.143,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.555,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 8.693,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.225,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.264,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.045,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.641,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.735,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.962,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.04,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.5,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.736,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.031,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.752,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.623,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.529,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.989,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.112,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.509,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.767,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.033,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.706,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.431,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.964,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.325,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.349,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.325,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.325,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.325,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.349,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.325,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.238,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.349,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "40b6e89b7826cd8a3e6c1b1ff0d272a7f705df19",
          "message": "chore: release v0.0.15 (#98)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-10T01:47:52+03:00",
          "tree_id": "1e213d215fb3f30011764f152f1161de3831ab5e",
          "url": "https://github.com/structured-world/structured-zstd/commit/40b6e89b7826cd8a3e6c1b1ff0d272a7f705df19"
        },
        "date": 1775779177939,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.067,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.016,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.047,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.087,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.047,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.039,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.047,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.317,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 18.699,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.854,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 143.04,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.354,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 80.81,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.774,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 72.42,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.89,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 103.583,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 20.914,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.504,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 12.088,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.252,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.593,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.626,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 6.449,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.735,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.255,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.82,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.596,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.353,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.899,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.172,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.449,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.409,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.896,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.155,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.959,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.24,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.474,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.416,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.859,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.604,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.033,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.93,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.363,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.355,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "8cef003c2c9fe1837518b3fc5c5b0c730b5d9bd0",
          "message": "perf(encoding): early incompressible fast-path + benchmark parity (#99)\n\n* perf(encoding): add early raw fast-path for incompressible blocks\n\n- add heuristic early no-compress path for fastest/default/level<=3\n\n- add dfast lazy-skipping in skip_matching for incompressible blocks\n\n- preserve boundary seeding for cross-block matching and add coverage\n\n- keep roundtrip + cross-validation + benchmark compatibility\n\n* fix(encoding): align incompressible fast-path review feedback\n\n- factor incompressible heuristic into shared helper + constants\n\n- switch heuristic sampling to head+tail to avoid prefix-only bias\n\n- keep dictionary priming on dense dfast seeding path\n\n- extend compressible negative-path tests for default and level(3)\n\n* fix(encoding): harden incompressible metrics and dfast skip path\n\n- avoid repeat sentinel collisions via explicit slot occupancy tracking\n\n- thread precomputed incompressible hints through matcher skip API\n\n- run dense tail reseed only for sparse dfast insertion path\n\n- add regression test for sparse incompressible cross-block tail matching\n\n* fix(encoding): preserve dense dictionary priming backfill\n\n* fix(encoding): tighten raw fast-path hint coverage\n\n* perf(best): add donor-style incompressible exits\n\n- enable and gate best-level raw early exits for incompressible data\n\n- preserve large-window match advantage with window-aware guard\n\n- propagate incompressible hints to HC skip path with sparse indexing\n\n- add regression tests for best fast-path and cross-block tail behavior\n\n* fix(encoding): backfill dfast dense skip tail\n\n- seed previous-slice dense tail in dfast skip_matching non-sparse path\n\n- add regression for dense skip cross-boundary tail matching\n\n- decouple sparse skip regression from incompressible classifier heuristic\n\n- document compact occupancy bitset stack tradeoff\n\n* refactor(encoding): share better-window cutoff constant\n\n- export BETTER_WINDOW_SIZE_BYTES from matcher level tuning\n\n- use shared constant in best raw fast-path gate to avoid drift\n\n* fix(encoding): harden raw fast-path review findings\n\n- gate Best raw fast-path eligibility by window size in helper API\n\n- short-circuit strict incompressible probes for minimum-size overlap\n\n- keep HC stepped insertion overflow-safe and add regression coverage\n\n* refactor(encoding): dedupe best gate and dfast tail seeding\n\n- remove redundant Best window re-check in raw fast-path selector\n\n- backfill Dfast boundary tail for sparse and dense skip paths\n\n- document boundary tail intent with named constant\n\n* perf(encoding): resolve review threads and align bench framing\n\n* perf(bench): scope ffi window_log to tiny inputs\n\n- apply window_log(14) parity only for <=16 KiB inputs in compare_ffi\n\n- keep larger FFI inputs on default level-driven window sizing\n\n- deduplicate high-entropy test fixture generator in match_generator tests\n\nCloses #97\n\n* perf(encoding): fix sparse-tail root causes and tiny header parity\n\n- default tiny hinted raw-only frames now emit single_segment=true for Rust/FFI header parity\n\n- HC sparse skip avoids reinserting step-grid tail positions; add regression test for self-loop prevention\n\n- add positive raw-fast-path success test (Raw block + Some(true) hint)\n\n- reduce incompressible classifier repeat-table stack footprint for no_std/small-stack targets\n\nCloses #97\n\n* chore(clippy): use is_multiple_of in match_generator\n\n* fix(encoding): restore matcher compatibility and strict probe test\n\n* refactor(encoding): centralize window and skip-step constants\n\n* fix(encoding): avoid overlapping strict probes on small blocks\n\n* fix(encoding): probe middle in capped sample\n\n* perf(encoding): broaden incompressible raw fast-path\n\n* fix(encoding): align default parity aliases and review semantics\n\n* test(encoding): cover 16KiB single-segment cutoff matrix\n\n* perf(encoding): add donor-style default fast parse loop\n\n* perf(encoding): unify default fast loop path\n\n* fix(encoding): honor row incompressible skip hint\n\n* refactor(encoding): return emitted block type\n\n* fix(encoding): align hinted single-segment parity across levels\n\n* fix(encoding): align hinted framing and matcher tail seeding\n\n* fix(encoding): align single-segment compat floor at 512\n\n* fix(encoding): harden hinted parity and dfast dense backfill\n\n* test(encoding): pin 511/512 header floor and tail seeding\n\n* fix(encoding): enforce donor parity gates and tail seeding",
          "timestamp": "2026-04-11T12:32:26+03:00",
          "tree_id": "e1bacbacaa0df16fd6e736c5834555a591d8a4d1",
          "url": "https://github.com/structured-world/structured-zstd/commit/8cef003c2c9fe1837518b3fc5c5b0c730b5d9bd0"
        },
        "date": 1775901819284,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.03,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.084,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.011,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.054,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 21.221,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.964,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 126.455,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.814,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 78.482,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 13.046,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 63.602,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.318,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 85.821,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 19.121,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.577,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.379,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.633,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.802,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 5.53,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.327,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.723,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.246,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.836,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.296,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.998,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.217,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.536,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.278,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.095,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.905,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.622,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.052,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.05,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.236,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.494,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.283,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.105,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.877,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.606,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.936,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.028,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.369,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.357,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "ae029067c91d79d61cb17414a3f3c02a3316cda8",
          "message": "chore: release v0.0.16 (#101)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-11T13:17:05+03:00",
          "tree_id": "87e33675a04f85cc77f96ad599df0c6169843cf6",
          "url": "https://github.com/structured-world/structured-zstd/commit/ae029067c91d79d61cb17414a3f3c02a3316cda8"
        },
        "date": 1775904505528,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.03,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.08,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 21.341,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.92,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 120.357,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.457,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 81.642,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.639,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 68.872,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.322,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 112.506,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 21.714,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.575,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.242,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 12.287,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.355,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.747,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.8,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 5.554,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.373,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.144,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.236,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.786,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.298,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.01,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.226,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.546,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.306,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.098,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.903,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.63,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.02,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.054,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.229,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.494,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.276,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.105,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.871,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.93,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.029,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e8ad7c10e6834dd788f5562da651abbafcda2d76",
          "message": "perf(encoding): CRC-gated hash mix for ARM and x86_64 (#102)\n\n* perf(encoding): gate hash mix with arm crc intrinsics\n\n- add aarch64 crc-enabled hash mixing path with runtime/static feature detection\n\n- keep multiplicative fallback for non-crc and non-arm targets\n\n- apply shared hash mix in dfast and row hash indexing\n\n- align row hash test with shared hash-mix contract\n\n* perf(encoding): add x86_64 sse4.2 crc hash path\n\n- extend hash mix with runtime-gated SSE4.2 CRC32 on x86_64\n\n- keep scalar fallback and existing aarch64 CRC path\n\n- add CPU-gated determinism tests for crc hash paths\n\n* fix(lint): resolve clippy warnings in crc hash gating\n\n- remove needless returns in feature-detection helpers\n\n- drop redundant u64 cast in sse4.2 crc mix path\n\n* test(encoding): validate crc-gated hash paths hit accelerated impl\n\n* perf(encoding): cache hash-mix kernel selection in matcher hot path\n\n* test(encoding): serialize and panic-proof hash kernel override\n\n* test(encoding): lock row hash extraction test against kernel override\n\n* perf(encoding): move hash kernel dispatch into matcher state\n\n* test(encoding): tighten kernel dispatch assertions",
          "timestamp": "2026-04-11T15:31:13+03:00",
          "tree_id": "8f09e4ce3907f7fb9abaf9f09bbc617fbb708f91",
          "url": "https://github.com/structured-world/structured-zstd/commit/e8ad7c10e6834dd788f5562da651abbafcda2d76"
        },
        "date": 1775912562753,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.031,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.06,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.011,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.069,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.061,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.016,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 21.213,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.448,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 125.138,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.125,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 77.739,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.336,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 61.685,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.283,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 84.052,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.707,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.417,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.242,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 15.426,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.288,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.296,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.744,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 9.302,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.293,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.38,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.111,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.659,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.616,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.789,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.041,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.132,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.555,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.827,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.112,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.756,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.625,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.071,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.133,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.512,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.81,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.114,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.726,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.616,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.528,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.046,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.346,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.346,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.348,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.338,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.348,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.337,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.348,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.337,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.348,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "b850c045522692f81de167a751428846ec074122",
          "message": "perf(encoding): complete ARM histogram path for #71 (#104)\n\n* perf(encoding): use donor-style striped histogram count\n\n* perf(encoding): gate histogram path on aarch64 sve2\n\n* fix(histogram): resolve review thread regressions\n\n* perf(histogram): harden and streamline hot path\n\n* test(histogram): avoid 32-bit overflow in sum test\n\n* test(histogram): avoid overflow in lane-merge test\n\n* perf(histogram): harden 32-bit hot loop\n\n* fix(fse): keep byte-table helper crate-local\n\n* fix(histogram): guard 16-byte loop bounds",
          "timestamp": "2026-04-11T18:32:17+03:00",
          "tree_id": "5c2df7d9b5411b645dd78bdb094ff8ed3ce94363",
          "url": "https://github.com/structured-world/structured-zstd/commit/b850c045522692f81de167a751428846ec074122"
        },
        "date": 1775923399636,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.028,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.087,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.05,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.054,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.05,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.016,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 21.378,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.139,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 138.665,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.334,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 92.5,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 11.88,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 78.977,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.46,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 112.953,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 24.604,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.618,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.244,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 19.848,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.323,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.127,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.838,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 6.84,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.374,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.34,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.333,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.731,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.624,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.566,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.962,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.241,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.566,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.78,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.051,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.834,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.64,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.613,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.016,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.523,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.795,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.055,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.816,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.627,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 5.514,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.991,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.199,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.35,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.2,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.201,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.349,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.199,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.337,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.201,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.35,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.2,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.201,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.358,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.199,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.337,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.201,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.349,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.2,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "491b2eb5a11f87ecb61fe7744d4554c7d3aeb7ee",
          "message": "chore: release v0.0.17 (#103)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-11T19:36:04+03:00",
          "tree_id": "63b677b8aa96b58ecec5701540eb0847d84215ae",
          "url": "https://github.com/structured-world/structured-zstd/commit/491b2eb5a11f87ecb61fe7744d4554c7d3aeb7ee"
        },
        "date": 1775927261468,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.031,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.098,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.058,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.07,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.059,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.016,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 18.782,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.771,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 118.537,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.812,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 72.563,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.373,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 61.212,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.426,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 77.98,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.859,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.466,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.225,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 14.328,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.33,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.244,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.741,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 10.973,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.351,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.403,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.144,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.649,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.61,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.766,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.042,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.117,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.554,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.764,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.112,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.752,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.626,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.513,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.07,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.125,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.511,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.753,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.114,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.719,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.617,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.421,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.344,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.272,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "491b2eb5a11f87ecb61fe7744d4554c7d3aeb7ee",
          "message": "chore: release v0.0.17 (#103)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-11T19:36:04+03:00",
          "tree_id": "63b677b8aa96b58ecec5701540eb0847d84215ae",
          "url": "https://github.com/structured-world/structured-zstd/commit/491b2eb5a11f87ecb61fe7744d4554c7d3aeb7ee"
        },
        "date": 1775929703684,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.031,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.092,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.058,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.07,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.059,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 21.069,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.465,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 116.408,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.193,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 73.017,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.422,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 61.62,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.384,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 81.418,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.804,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.406,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.215,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 12.823,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.319,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.207,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.738,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 9.218,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.35,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.299,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.094,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.649,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.613,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.737,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.041,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.12,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.555,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.746,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.114,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.747,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.623,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.515,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.07,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.122,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.512,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.751,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.116,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.72,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.615,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.414,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.044,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.338,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.343,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.269,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.271,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "495a38d04b6f069c2dd3ef72a09029e790131fb8",
          "message": "perf(decoding): add shared dictionary handle (#105)\n\n* perf(decoding): add shared dictionary handle\n\n* perf(decoding): add prepared dict decode path\n\n* fix(decoding): address review feedback\n\n- use Rc on non-atomic targets for DictionaryHandle\n- keep add_dict allocation-free and add dict decode tests\n- add benchmark preflight correctness checks\n\n* refactor(decoding): share decode loop\n\n- reuse shared multi-frame decode helper\n- ensure sample dict fixture requires a dictionary\n\n* test(decoding): raise patch coverage\n\n- cover decode_all skip and target-too-small paths\n- exercise DictionaryHandle conversions\n\n* test(decoding): cover dict handle edges\n\n- cover add_dict_from_bytes and mismatched dict handle reset\n- exercise dictionary handle as_ref via into_handle\n- document decode_all skipping skippable frames\n\n* test(decoding): assert decode_all written length\n\n- assert decode_all_* returns original length\n- guard against zero-filled false positives\n\n* fix(decoding): guard dict handle usage\n\n- prevent duplicate dict ids across owned/shared maps\n- gate shared dict storage on atomic targets\n- assert decode_all writes full length and harden bench allocations\n\n* docs(readme): document dictionary decode api and strategy gaps\n\n- add dictionary-backed decode examples for handle and raw-bytes paths\n- clarify implemented compression backend coverage by level\n- list currently unimplemented strategy families (greedy, btopt, btultra)\n\n* fix(decoding): keep no_std ptr path and update dict error text\n\n* fix(decoding): clarify dict safety note and error guidance\n\n* fix(decoding): clarify dict mismatch and force_dict preconditions\n\n* fix(decoding): apply explicit dict when frame omits dict id\n\n* docs(decoding): document duplicate dict registration errors\n\n* refactor(decoding): remove unsafe dictionary borrows\n\n* fix(decoding): gate shared dictionary lookups\n\n* test(decoding): add force_dict regression coverage\n\n* refactor(decoding): remove unsafe force_dict lookup\n\n* fix(decoding): remove unsafe reset dict lookup\n\n* docs(decoding): warn about dict-handle misuse\n\n* docs(decoding): mirror dict-handle safety warnings\n\n* refactor(decoding): unify shared dict accessor naming\n\n* fix(decoding): validate registered dictionary invariants\n\n* fix(decoding): validate dict handles before reset\n\n* style(decoding): unify DictionaryHandle accessor usage\n\n* test(decoding): validate DictionaryHandle AsRef in unit test",
          "timestamp": "2026-04-12T13:14:10+03:00",
          "tree_id": "7528a60734c1b6285b1841f2fdaac375b7f047af",
          "url": "https://github.com/structured-world/structured-zstd/commit/495a38d04b6f069c2dd3ef72a09029e790131fb8"
        },
        "date": 1775990628246,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.023,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.071,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.04,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.044,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.041,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.013,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 17.521,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.908,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 115.148,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.725,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 66.802,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 9.916,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 55.268,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.033,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 76.118,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 14.814,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.204,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 9.832,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.253,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 3.645,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.558,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 5.231,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.267,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 3.668,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 0.861,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.174,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.468,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.592,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.768,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.732,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.41,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.769,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.843,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.253,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.477,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.635,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.811,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.744,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.375,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.787,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.847,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.228,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.471,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 4.553,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.789,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.277,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.286,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.278,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.205,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.285,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.278,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.205,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.286,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.278,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.205,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.286,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.278,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.205,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.286,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.202,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "051fed2d82168c18d844555919bc22ccd06bc2f5",
          "message": "chore: release v0.0.18 (#106)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-12T17:46:13+03:00",
          "tree_id": "4c5ccf5de2787d0f9d29b57a35a8ed5c16372acf",
          "url": "https://github.com/structured-world/structured-zstd/commit/051fed2d82168c18d844555919bc22ccd06bc2f5"
        },
        "date": 1776007043843,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.03,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.097,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.058,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.069,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.059,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 18.886,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.766,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 125.071,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.162,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 71.105,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.159,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 61.808,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 80.268,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 17.986,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.424,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.232,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 14.303,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.293,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.378,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.647,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 9.417,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.311,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.778,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.025,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.657,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.897,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.041,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.266,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.553,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.889,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.113,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.741,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.625,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.636,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.071,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.115,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.51,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.884,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.118,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.716,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.616,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.558,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.346,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.342,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.334,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.345,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.342,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.342,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.353,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fe27fc6678e6985dd23bb2f6262716910d4574c5",
          "message": "perf(bench): add wildcopy candidate research bench and dashboard range filter (#107)\n\n* feat(bench-dashboard): add snapshot range filter and taller chart\n\n* perf(bench): add wildcopy candidate benchmark harness\n\n* fix(bench): address review feedback and CI lint\n\n- keep bench internals exposed only via structured_zstd::testing\n\n- select candidate kernel once and harden benchmark against copy elision\n\n- document #87 decision mapping and link follow-up issue #108\n\n* fix(bench): align candidate benchmark paths\n\n- benchmark baseline and candidate with symmetric overshoot policy\n\n- add non-aligned length coverage and tighten bench shim visibility\n\n* fix(bench): align baseline dispatcher with production\n\n- baseline selector now mirrors avx512/avx2/sse2/scalar thresholds\n\n- include sub-64 lengths and keep bench shim only under testing facade\n\n* refactor(bench): return BenchPath from candidate selector\n\n* fix(bench): select candidate by len and gate x86 bench\n\n* fix(bench): align candidate overshoot with baseline policy\n\n* fix(bench): add explicit unsafe blocks in wildcopy bench\n\n* fix(bench): address wildcard review findings\n\n* fix(bench): keep bench shim public without re-export leak\n\n* fix(bench): narrow bench shim visibility in simd copy",
          "timestamp": "2026-04-12T21:11:02+03:00",
          "tree_id": "803684355ec5dcfc78151543e2dc7566b9c1665d",
          "url": "https://github.com/structured-world/structured-zstd/commit/fe27fc6678e6985dd23bb2f6262716910d4574c5"
        },
        "date": 1776019328535,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.031,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.095,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.059,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.07,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.061,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.017,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 21.396,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.428,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 131.186,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.166,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 82.595,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 12.329,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 62.607,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.276,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 86.571,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 18.077,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.413,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.232,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 14.125,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.292,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 5.308,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.64,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 9.821,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.308,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 5.473,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 0.991,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.653,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.608,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.888,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.042,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.103,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.553,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.867,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.117,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.739,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.623,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.617,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.074,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.111,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.508,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.228,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.116,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.71,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.612,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.53,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.045,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.346,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.336,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.237,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.347,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.27,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "95af26de283783357cb40a1f39532aa3e0cef0a1",
          "message": "chore: release v0.0.19 (#109)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-04-12T22:12:33+03:00",
          "tree_id": "a33f83203ee1d8e2a1e494f4214eb107bc2ff3f0",
          "url": "https://github.com/structured-world/structured-zstd/commit/95af26de283783357cb40a1f39532aa3e0cef0a1"
        },
        "date": 1776023005108,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/pure_rust",
            "value": 0.03,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/pure_rust",
            "value": 0.092,
            "unit": "ms"
          },
          {
            "name": "compress/default/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/better/small-4k-log-lines/matrix/c_ffi",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/pure_rust",
            "value": 0.057,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/pure_rust",
            "value": 0.054,
            "unit": "ms"
          },
          {
            "name": "compress/best/small-4k-log-lines/matrix/c_ffi",
            "value": 0.016,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/pure_rust",
            "value": 20.708,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.979,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/pure_rust",
            "value": 133.875,
            "unit": "ms"
          },
          {
            "name": "compress/default/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.82,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/pure_rust",
            "value": 87.114,
            "unit": "ms"
          },
          {
            "name": "compress/better/decodecorpus-z000033/matrix/c_ffi",
            "value": 13.234,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/pure_rust",
            "value": 91.83,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.214,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/pure_rust",
            "value": 133.555,
            "unit": "ms"
          },
          {
            "name": "compress/best/decodecorpus-z000033/matrix/c_ffi",
            "value": 19.271,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/pure_rust",
            "value": 1.576,
            "unit": "ms"
          },
          {
            "name": "compress/fastest/low-entropy-1m/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/pure_rust",
            "value": 13.221,
            "unit": "ms"
          },
          {
            "name": "compress/default/low-entropy-1m/matrix/c_ffi",
            "value": 0.326,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/pure_rust",
            "value": 4.66,
            "unit": "ms"
          },
          {
            "name": "compress/better/low-entropy-1m/matrix/c_ffi",
            "value": 0.718,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/pure_rust",
            "value": 15.864,
            "unit": "ms"
          },
          {
            "name": "compress/level4-row/low-entropy-1m/matrix/c_ffi",
            "value": 0.346,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/pure_rust",
            "value": 4.858,
            "unit": "ms"
          },
          {
            "name": "compress/best/low-entropy-1m/matrix/c_ffi",
            "value": 1.11,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/default/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/better/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "decompress/best/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.783,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.606,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.201,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.981,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.217,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.536,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.201,
            "unit": "ms"
          },
          {
            "name": "decompress/default/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.088,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.888,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.618,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.999,
            "unit": "ms"
          },
          {
            "name": "decompress/better/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.282,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.485,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 7.2,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.095,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.907,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.603,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 6.856,
            "unit": "ms"
          },
          {
            "name": "decompress/best/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.017,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/fastest/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.261,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.366,
            "unit": "ms"
          },
          {
            "name": "decompress/default/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.261,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.355,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.266,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.365,
            "unit": "ms"
          },
          {
            "name": "decompress/better/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.261,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.355,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.367,
            "unit": "ms"
          },
          {
            "name": "decompress/level4-row/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.26,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.354,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.265,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.364,
            "unit": "ms"
          },
          {
            "name": "decompress/best/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.261,
            "unit": "ms"
          }
        ]
      }
    ]
  }
}