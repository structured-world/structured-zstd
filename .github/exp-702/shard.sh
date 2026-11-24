#!/bin/sh
# One dashboard shard exactly as CI runs it (run-benchmarks.sh: 16 MiB large
# inputs, three rounds with alternating arm order, every scenario), printing
# the CPU first and the paired lines of each round.
# Usage: shard.sh <comma-separated levels>
set -eu
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "thp $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
export STRUCTURED_ZSTD_BENCH_TARGET=x86_64-gnu
export STRUCTURED_ZSTD_BENCH_TRIPLE=x86_64-unknown-linux-gnu
export STRUCTURED_ZSTD_BENCH_LEVEL_FILTER="$1"
export STRUCTURED_ZSTD_BENCH_CORPUS_PATH="$PWD/zstd/decodecorpus_files/z000033"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
bash .github/scripts/run-benchmarks.sh > out.txt 2>&1 || { cat out.txt; exit 1; }
grep -E '^(REPORT_PAIR|Benchmark round)' out.txt
