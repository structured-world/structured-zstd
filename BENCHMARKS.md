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
- `structured-zstd::Best` vs `zstd` level `11`

Dictionary benchmarks currently include:

- C FFI `with_dict` vs `without_dict` compression runs
- dictionary training timing comparison (`dict-train`) between Rust FastCOVER and C FFI trainer

## Issue #24 Acceptance Mapping

- [x] Criterion benchmarks for compress/decompress at all currently implemented levels
- [x] Comparison against C zstd at same levels
- [x] Flamegraph generation script (`scripts/bench-flamegraph.sh`)
- [x] Small data (`1-10 KiB`) scenarios for CoordiNode-like payloads
- [x] Results documented in `benchmark-report.md`

## Issue #87 Research Mapping (Wildcopy Candidates)

Research PR: [#107](https://github.com/structured-world/structured-zstd/pull/107)

- [x] At least one candidate benchmarked against baseline on supported hardware.
- [x] No correctness regression in research branch validation runs.
- [x] Candidate shows reproducible gain in local sample runs.
- [x] Final recommendation documented with go/no-go outcome.

Local sample measurements from `wildcopy_candidates` (ns/iter):

- `64B`: baseline `3` -> candidate `2`
- `256B`: baseline `7` -> candidate `4`
- `1024B`: baseline `28` -> candidate `14`
- `4096B`: baseline `94` -> candidate `58`
- `16384B`: baseline `347` -> candidate `268`
- `65536B`: baseline `1368` -> candidate `1121`

Decision: **Provisional GO (microbench only)** for AVX2 unroll2 candidate,
based on local `wildcopy_candidates` microbench data; this does not close issue #87.

Follow-up implementation issue: [#108](https://github.com/structured-world/structured-zstd/issues/108).

## Commands

Run the full Criterion matrix:

```bash
cargo bench --bench compare_ffi -p structured-zstd --features dict_builder -- --output-format bencher
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

- `benchmark-results.json` for GitHub regression tracking (smoke subset only):
  - stages: `compress`, `decompress`
  - levels: `default`, `better`
  - scenarios: `small-4k-log-lines`, `decodecorpus-z000033` (or fallback `decodecorpus-synthetic-1m`), `low-entropy-1m`
- `benchmark-report.md` with:
  - compression ratio tables (`REPORT`)
  - input+output buffer size estimate tables (`REPORT_MEM`)
  - dictionary compression tables (`REPORT_DICT`)
  - dictionary training comparison tables (`REPORT_DICT_TRAIN`)
  - timing rows for all benchmark functions
- `benchmark-delta.json` with canonical `(scenario + params)` rows including:
  - raw Rust/FFI ratio values and `rust/ffi` ratio delta
  - raw Rust/FFI speed values (`bytes/sec`) and `rust/ffi` speed delta
- `benchmark-delta.md` with two packs:
  - Ratio pack: Rust ratio, FFI ratio, Rust/FFI ratio delta
  - Speed pack: Rust speed, FFI speed, Rust/FFI speed delta
- `benchmark-relative.json` with normalized relative rows:
  - dimensions: `target`, `stage`, `scenario`, `level`, optional `source`
  - metrics: `rust_value`, `ffi_value`, `delta_ratio`, `delta_percent`, `status_band`
  - metric kinds: `compression_ratio`, `throughput_bytes_per_sec`

## CI Target Matrix

GitHub Actions runs the benchmark suite in an explicit target matrix:

- `x86_64-unknown-linux-gnu` (`x86_64-gnu`)
- `i686-unknown-linux-gnu` (`i686-gnu`)
- `x86_64-unknown-linux-musl` (`x86_64-musl`)

Each matrix run tags output rows with target metadata and publishes target-scoped artifacts. On `main`,
the pipeline merges matrix artifacts into canonical `gh-pages/dev/bench/benchmark-relative.json` and
`benchmark-delta.json`.

`github-action-benchmark` regression checks are intentionally **advisory** on PRs because GitHub-hosted
runners have high run-to-run CPU variance. The blocking signal remains functional correctness checks;
performance alerts are used as triage prompts for human review.

Delta interpretation (direct same-run comparison on the same environment):

- **Ratio delta** (`rust_ratio / ffi_ratio`): lower is better for Rust
- **Speed delta**: higher is better for Rust
  - throughput form: `rust_bytes_per_sec / ffi_bytes_per_sec`
  - fallback form (when throughput is unavailable): `ffi_ms_per_iter / rust_ms_per_iter`

Status labels in `benchmark-delta` are derived directly from the same-run deltas (no environment
calibration/pre-test coefficients):

- **ratio status**: `rust_better_smaller` when `< 0.99`, `near_parity` when `0.99..=1.05`, `rust_worse_larger` when `> 1.05`
- **speed status**: `rust_faster` when `> 1.05`, `near_parity` when `0.99..=1.05`, `rust_slower` when `< 0.99`

Criterion also writes its usual detailed estimates under `target/criterion/`.
