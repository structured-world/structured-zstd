#!/bin/sh
# One i686 dashboard shard as CI runs it (run-benchmarks.sh on the prebuilt
# compare_ffi binary: 16 MiB large inputs, three rounds with alternating arm
# order, every scenario), printing the CPU first and the paired lines of each
# round. Usage: shard.sh <comma-separated levels>
set -eu
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "family $(grep -m1 'cpu family' /proc/cpuinfo | cut -d: -f2-) model $(grep -m1 '^model[[:space:]]*:' /proc/cpuinfo | cut -d: -f2-)"
cargo bench -q -p ffi-bench --bench compare_ffi --target i686-unknown-linux-gnu --no-run --message-format=json > build.json
bin=$(jq -r 'select(.executable != null and .target.name == "compare_ffi") | .executable' build.json | tail -1)
export STRUCTURED_ZSTD_BENCH_BIN="$bin"
export STRUCTURED_ZSTD_BENCH_TARGET=i686-gnu
export STRUCTURED_ZSTD_BENCH_TRIPLE=i686-unknown-linux-gnu
export STRUCTURED_ZSTD_BENCH_LEVEL_FILTER="$1"
export STRUCTURED_ZSTD_BENCH_CORPUS_PATH="$PWD/zstd/decodecorpus_files/z000033"
bash .github/scripts/run-benchmarks.sh > out.txt 2>&1 || { cat out.txt; exit 1; }
grep -E '^(REPORT_PAIR|Benchmark round)' out.txt
jq -c '.[] | select(.name | test("small-10k-random"))' benchmark-results.json
