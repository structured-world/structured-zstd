#!/bin/sh
# The compression cells that lose only on hosted runners, through the
# dashboard's compare_ffi with its build flags. Each cell is paired against
# libzstd inside its own run. Usage: cells.sh <target triple>
set -eu
target=$1
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "family $(grep -m1 'cpu family' /proc/cpuinfo | cut -d: -f2-) model $(grep -m1 '^model[[:space:]]*:' /proc/cpuinfo | cut -d: -f2-)"
echo "thp $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
export STRUCTURED_ZSTD_EMIT_REPORT=1
if [ "$target" = x86_64-unknown-linux-gnu ]; then
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
  export STRUCTURED_ZSTD_BENCH_LEVEL_FILTER=level_-5_fast,level_-2_fast,level_1_fast,level_3_dfast,level_16_btopt
  filter='^compress/level_(-5_fast|-2_fast|1_fast|3_dfast|16_btopt)/(low-entropy-1m|large-log-stream)'
else
  export STRUCTURED_ZSTD_BENCH_LEVEL_FILTER=level_3_dfast,level_11_lazy,level_12_lazy,level_13_lazy
  filter='^(compress|compress-dict)/level_(3_dfast|1[1-3]_lazy)/small-10k-random'
fi
cargo bench -q -p ffi-bench --bench compare_ffi --target "$target" -- "$filter" > out.txt 2>&1 || { cat out.txt; exit 1; }
grep -E '^REPORT_PAIR' out.txt
