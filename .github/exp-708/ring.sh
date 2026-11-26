#!/bin/bash
# Prepared-dictionary frames with a fresh context per frame on one hosted
# runner: the z000033 training samples compressed with a dictionary trained on
# them at levels 1, 5, 12 and 19, built from the parent of the first commit
# that lends a prepared dictionary's tables (base) and from this commit
# (head), and libzstd, interleaved rounds of prebuilt binaries. Rounds are
# prefixed with the level ("l5r3"). Every run's wall time printed raw, in
# nanoseconds.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
first=$(git log --format=%H --grep='^perf(encoding): prepared dictionaries lend their dfast tables' -1)
base=$(git rev-parse "$first^")
echo "head $(git rev-parse HEAD) base $base"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
mkdir -p bin
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop --example train_loop_fastcover
cp target/release/examples/dict_samples_loop bin/head
target/release/examples/train_loop_fastcover rust 1 zstd/decodecorpus_files/z000033 dict.bin
git checkout -q "$base" -- zstd/src
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop
cp target/release/examples/dict_samples_loop bin/base
git checkout -q HEAD -- zstd/src
corpus=zstd/decodecorpus_files/z000033
for level in 1 5 12 19; do
  iters=40
  [ "$level" -ge 12 ] && iters=10
  for round in $(seq 1 5); do
    for arm in base head c; do
      if [ "$arm" = c ]; then bin=bin/head; side=c-fresh; else bin=bin/$arm; side=rust-fresh; fi
      start=$(date +%s%N)
      "$bin" "$side" "$level" "$iters" "$corpus" dict.bin 2>/dev/null
      end=$(date +%s%N)
      echo "l${level}r$round $arm $((end - start))"
    done
  done
done
