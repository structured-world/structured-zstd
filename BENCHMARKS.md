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

The local default for the large scenario is `100 MiB`. CI can override it with
`STRUCTURED_ZSTD_BENCH_LARGE_BYTES` to keep regression runs bounded while still exercising the
same code path.

## Level Mapping

The benchmark suite only compares levels that are currently implemented end-to-end in the pure Rust
encoder:

- `structured-zstd::Fastest` vs `zstd` level `1`
- `structured-zstd::Default` vs `zstd` level `3`

`Better` and `Best` are intentionally excluded until the encoder implements them. Dictionary
compression is also excluded from the timing matrix because the crate currently exposes dictionary
training, but not dictionary-based compression.

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
bash scripts/bench-flamegraph.sh decompress/default/decodecorpus-z000033/matrix/pure_rust
```

## Outputs

`run-benchmarks.sh` writes:

- `benchmark-results.json` for GitHub regression tracking
- `benchmark-report.md` with scenario-by-scenario compression ratios and timing rows

Criterion also writes its usual detailed estimates under `target/criterion/`.
