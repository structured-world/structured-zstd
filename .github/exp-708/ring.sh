#!/bin/bash
# The whole PR on one hosted runner: main (base) against this commit (head)
# and libzstd, interleaved rounds of prebuilt binaries.
# - FastCOVER training of z000033 ("train"), ten trainings a run.
# - Prepared-dictionary frames of its training samples at a level of every
#   backend, with a reused context ("r") and a fresh context per frame ("f").
# Rounds are prefixed with what they time ("train3", "l5fr2"). Every run's
# wall time printed raw, in nanoseconds.
set -euo pipefail
base=$(git merge-base HEAD origin/main)
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD) base $base"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
mkdir -p bin
examples="--example dict_samples_loop --example train_loop_fastcover"
cargo build -q --profile bench -p ffi-bench $examples
cp target/release/examples/dict_samples_loop bin/head
cp target/release/examples/train_loop_fastcover bin/train-head
target/release/examples/train_loop_fastcover rust 1 zstd/decodecorpus_files/z000033 dict.bin
git checkout -q "$base" -- zstd/src
cargo build -q --profile bench -p ffi-bench $examples
cp target/release/examples/dict_samples_loop bin/base
cp target/release/examples/train_loop_fastcover bin/train-base
git checkout -q HEAD -- zstd/src
corpus=zstd/decodecorpus_files/z000033
for round in $(seq 1 5); do
  for arm in base head c; do
    if [ "$arm" = c ]; then bin=bin/train-head; side=c; else bin=bin/train-$arm; side=rust; fi
    start=$(date +%s%N)
    "$bin" "$side" 10 "$corpus" 2>/dev/null
    end=$(date +%s%N)
    echo "train$round $arm $((end - start))"
  done
done
for level in 1 3 5 9 12 19; do
  iters=100
  [ "$level" -ge 12 ] && iters=10
  for mode in r f; do
    suffix=""
    [ "$mode" = f ] && suffix="-fresh"
    for round in $(seq 1 5); do
      for arm in base head c; do
        if [ "$arm" = c ]; then bin=bin/head; side="c$suffix"; else bin=bin/$arm; side="rust$suffix"; fi
        start=$(date +%s%N)
        "$bin" "$side" "$level" "$iters" "$corpus" dict.bin 2>/dev/null
        end=$(date +%s%N)
        echo "l$level${mode}r$round $arm $((end - start))"
      done
    done
  done
done
