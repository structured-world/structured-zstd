#!/bin/bash
# Prepared-dictionary frames of the z000033 training samples at level 1 on one
# hosted runner, a reused context: a time profile of each side (ours,
# libzstd) by symbol from the cpu-clock software event, which needs no
# hardware counters, and the annotate of each side's match loop. Every perf
# output printed whole.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD)"
sudo apt-get -qq update
sudo apt-get -qq install -y "linux-tools-$(uname -r)" linux-tools-generic || sudo apt-get -qq install -y linux-tools-azure
sudo sysctl -w kernel.perf_event_paranoid=-1
sudo sysctl -w kernel.kptr_restrict=0
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop --example train_loop_fastcover
bin=target/release/examples/dict_samples_loop
corpus=zstd/decodecorpus_files/z000033
target/release/examples/train_loop_fastcover rust 1 "$corpus" dict.bin
for side in rust c; do
  echo "== time $side"
  for round in 1 2 3; do
    start=$(date +%s%N)
    "$bin" "$side" 1 100 "$corpus" dict.bin 2>/dev/null
    echo "r$round $side $(( $(date +%s%N) - start ))"
  done
  echo "== record $side"
  perf record -e cpu-clock -F 4999 -o "$side.data" -- "$bin" "$side" 1 400 "$corpus" dict.bin 2>&1
  echo "== self $side"
  perf report -i "$side.data" --stdio --no-children --sort symbol --percent-limit 0.5 2>&1
done
echo "== annotate rust"
perf annotate -i rust.data --stdio --no-source --percent-limit 20 2>&1
echo "== annotate c"
perf annotate -i c.data --stdio --no-source ZSTD_compressBlock_fast_dictMatchState 2>&1
