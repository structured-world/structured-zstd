window.BENCHMARK_DATA = {
  "lastUpdate": 1774413075371,
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
      }
    ]
  }
}