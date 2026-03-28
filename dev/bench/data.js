window.BENCHMARK_DATA = {
  "lastUpdate": 1774714145029,
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
      }
    ]
  }
}