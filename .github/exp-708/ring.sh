#!/bin/bash
# The scoring pass of FastCOVER training alone on one hosted runner: the
# z000033 training samples compressed with a dictionary trained on them,
# ours twice per round (base and head are the same binary, a noise control)
# and libzstd, interleaved rounds. Every run's wall time printed raw, in
# nanoseconds.
set -euo pipefail
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "family $(grep -m1 'cpu family' /proc/cpuinfo | cut -d: -f2-) model $(grep -m1 '^model[[:space:]]*:' /proc/cpuinfo | cut -d: -f2-)"
echo "head $(git rev-parse HEAD) base $(git rev-parse HEAD)"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
cargo build -q --profile bench -p ffi-bench --example dict_samples_loop --example train_loop_fastcover
bin=target/release/examples
corpus=zstd/decodecorpus_files/z000033
"$bin/train_loop_fastcover" rust 1 "$corpus" dict.bin
for round in $(seq 1 7); do
  for arm in base head c; do
    if [ "$arm" = c ]; then side=c; else side=rust; fi
    start=$(date +%s%N)
    "$bin/dict_samples_loop" "$side" 3 100 "$corpus" dict.bin 2>/dev/null
    end=$(date +%s%N)
    echo "r$round $arm $((end - start))"
  done
done
