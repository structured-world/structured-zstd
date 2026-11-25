#!/bin/bash
# FastCOVER training on z000033 on one hosted runner: the window with the ring
# (two commits back, before the no-ring experiment and this workflow) against
# the window rehashing the leaving dmer (this commit's source), and libzstd,
# interleaved rounds of prebuilt binaries. Every run's
# wall time printed raw, in nanoseconds.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "family $(grep -m1 'cpu family' /proc/cpuinfo | cut -d: -f2-) model $(grep -m1 '^model[[:space:]]*:' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD) base $(git rev-parse HEAD~2)"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
mkdir -p bin
cargo build -q --profile bench -p ffi-bench --example train_loop_fastcover
cp target/release/examples/train_loop_fastcover bin/head
git checkout -q HEAD~2 -- zstd/src
cargo build -q --profile bench -p ffi-bench --example train_loop_fastcover
cp target/release/examples/train_loop_fastcover bin/base
git checkout -q HEAD -- zstd/src
corpus=zstd/decodecorpus_files/z000033
for round in $(seq 1 7); do
  for arm in base head c; do
    if [ "$arm" = c ]; then bin=bin/head; side=c; else bin=bin/$arm; side=rust; fi
    start=$(date +%s%N)
    "$bin" "$side" 10 "$corpus" 2>/dev/null
    end=$(date +%s%N)
    echo "r$round $arm $((end - start))"
  done
done
