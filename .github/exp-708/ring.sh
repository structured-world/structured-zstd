#!/bin/bash
# Prepared-dictionary frames of the z000033 training samples, ours against
# libzstd, at the levels each backend runs, with a reused context ("r") and a
# fresh context per frame ("f"), on one hosted runner. Ours runs twice per
# round (base and head are the same binary, a noise control). Rounds are
# prefixed with the level and mode ("l5fr3"). Every run's wall time printed
# raw, in nanoseconds.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD) base $(git rev-parse HEAD)"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop --example train_loop_fastcover
bin=target/release/examples/dict_samples_loop
corpus=zstd/decodecorpus_files/z000033
target/release/examples/train_loop_fastcover rust 1 "$corpus" dict.bin
for level in 1 2 3 4 5 6 7 9 12 15 16 19 22; do
  iters=40
  [ "$level" -ge 5 ] && iters=20
  [ "$level" -ge 13 ] && iters=5
  [ "$level" -ge 20 ] && iters=2
  for mode in r f; do
    suffix=""
    [ "$mode" = f ] && suffix="-fresh"
    for round in $(seq 1 5); do
      for arm in base head c; do
        if [ "$arm" = c ]; then side="c$suffix"; else side="rust$suffix"; fi
        start=$(date +%s%N)
        "$bin" "$side" "$level" "$iters" "$corpus" dict.bin 2>/dev/null
        end=$(date +%s%N)
        echo "l$level${mode}r$round $arm $((end - start))"
      done
    done
  done
done
