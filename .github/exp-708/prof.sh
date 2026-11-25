#!/bin/bash
# FastCOVER training on z000033 on one hosted runner: a time profile of each
# side (ours, libzstd) by symbol, from the cpu-clock software event, which
# needs no hardware counters. Every perf output printed whole.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "family $(grep -m1 'cpu family' /proc/cpuinfo | cut -d: -f2-) model $(grep -m1 '^model[[:space:]]*:' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD)"
sudo apt-get -qq update
sudo apt-get -qq install -y "linux-tools-$(uname -r)" linux-tools-generic || sudo apt-get -qq install -y linux-tools-azure
sudo sysctl -w kernel.perf_event_paranoid=-1
sudo sysctl -w kernel.kptr_restrict=0
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3 -C force-frame-pointers=yes"
cargo build -q --profile bench -p ffi-bench --example train_loop_fastcover
bin=target/release/examples/train_loop_fastcover
corpus=zstd/decodecorpus_files/z000033
echo "== hardware counters"
perf stat -e cycles:u,instructions:u,branch-misses:u "$bin" rust 10 "$corpus" 2>&1 || true
for side in rust c; do
  echo "== time $side"
  for round in 1 2 3; do
    start=$(date +%s%N)
    "$bin" "$side" 10 "$corpus" 2>/dev/null
    echo "r$round $side $(( $(date +%s%N) - start ))"
  done
  echo "== record $side"
  perf record -e cpu-clock -F 1999 -g -o "$side.data" -- "$bin" "$side" 20 "$corpus" 2>&1
  echo "== self $side"
  perf report -i "$side.data" --stdio --no-children --sort symbol -g none --percent-limit 0.5 2>&1
  echo "== children $side"
  perf report -i "$side.data" --stdio --children --sort symbol -g none --percent-limit 3 2>&1
done
