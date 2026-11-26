#!/bin/bash
# Prepared-dictionary frames of the z000033 training samples at levels 1 and 2
# on one hosted runner, the source two commits back (base) against this
# commit's (head), and libzstd, with a reused context ("r") and a fresh
# context per frame ("f"), interleaved rounds of prebuilt binaries. Rounds
# are prefixed with the level and mode ("l1fr3"). Every run's wall time
# printed raw, in nanoseconds.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD) base $(git rev-parse HEAD~2)"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
mkdir -p bin
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop --example train_loop_fastcover
cp target/release/examples/dict_samples_loop bin/head
target/release/examples/train_loop_fastcover rust 1 zstd/decodecorpus_files/z000033 dict.bin
git checkout -q HEAD~2 -- zstd/src
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop
cp target/release/examples/dict_samples_loop bin/base
git checkout -q HEAD -- zstd/src
corpus=zstd/decodecorpus_files/z000033
for level in 1 2; do
  for mode in r f; do
    suffix=""
    [ "$mode" = f ] && suffix="-fresh"
    for round in $(seq 1 7); do
      for arm in base head c; do
        if [ "$arm" = c ]; then bin=bin/head; side="c$suffix"; else bin=bin/$arm; side="rust$suffix"; fi
        start=$(date +%s%N)
        "$bin" "$side" "$level" 100 "$corpus" dict.bin 2>/dev/null
        end=$(date +%s%N)
        echo "l$level${mode}r$round $arm $((end - start))"
      done
    done
  done
done
