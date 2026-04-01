# Benchmark Suite

`structured-zstd` keeps its compression/decompression performance tracking in the Criterion bench
matrix at `zstd/benches/compare_ffi.rs`.

## Scenarios

The current matrix covers:

- small random payloads (`1 KiB`, `10 KiB`)
- a small structured log payload (`4 KiB`)
- a repository corpus fixture (`decodecorpus_files/z000033`)
- high entropy random payloads (`1 MiB`)
- low entropy repeated payloads (`1 MiB`)
- a large structured stream (`100 MiB`)
- optional Silesia corpus files when `STRUCTURED_ZSTD_SILESIA_DIR=/path/to/silesia` is set
  - load is bounded by `STRUCTURED_ZSTD_SILESIA_MAX_FILES` (default `12`) and
    `STRUCTURED_ZSTD_SILESIA_MAX_FILE_BYTES` (default `67108864`)

For decompression, each scenario/level pair is benchmarked against two frame sources:

- `rust_stream`: frame produced by `structured-zstd`
- `c_stream`: frame produced by C `zstd`

This keeps Rust-vs-C decoder comparisons symmetric and catches format/interop drift sooner.

The local default for the large scenario is `100 MiB`. In GitHub Actions, when
`STRUCTURED_ZSTD_BENCH_LARGE_BYTES` is unset, `.github/scripts/run-benchmarks.sh` defaults it to
`16 MiB` to keep CI regression runs bounded while still exercising the same code path.

## Level Mapping

The benchmark suite only compares levels that are currently implemented end-to-end in the pure Rust
encoder:

- `structured-zstd::Fastest` vs `zstd` level `1`
- `structured-zstd::Default` vs `zstd` level `3`
- `structured-zstd::Better` vs `zstd` level `7`

`Best` is intentionally excluded until the encoder implements it.

Dictionary benchmarks are tracked separately with C FFI `with_dict` vs `without_dict` runs, using a
dictionary trained from scenario samples. Pure Rust dictionary compression is still pending and is
therefore not part of the pure-Rust-vs-C timing matrix yet.

## Issue #24 Acceptance Mapping

- [x] Criterion benchmarks for compress/decompress at all currently implemented levels
- [x] Comparison against C zstd at same levels
- [x] Flamegraph generation script (`scripts/bench-flamegraph.sh`)
- [x] Small data (`1-10 KiB`) scenarios for CoordiNode-like payloads
- [x] Results documented in `benchmark-report.md`

## Commands

Run the full Criterion matrix:

```bash
cargo bench --bench compare_ffi -p structured-zstd -- --output-format bencher
```

Generate the CI-style JSON and markdown report locally:

```bash
bash .github/scripts/run-benchmarks.sh
```

Generate a flamegraph for a hot path:

```bash
bash scripts/bench-flamegraph.sh
```

Override the benchmark targeted by the flamegraph script:

```bash
bash scripts/bench-flamegraph.sh decompress/default/decodecorpus-z000033/rust_stream/matrix/pure_rust
```

## Outputs

`run-benchmarks.sh` writes:

- `benchmark-results.json` for GitHub regression tracking
- `benchmark-report.md` with:
  - compression ratio tables (`REPORT`)
  - input+output buffer size estimate tables (`REPORT_MEM`)
  - dictionary compression tables (`REPORT_DICT`)
  - timing rows for all benchmark functions

Criterion also writes its usual detailed estimates under `target/criterion/`.
