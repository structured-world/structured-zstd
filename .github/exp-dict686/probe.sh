#!/bin/sh
# The decompress-dict small-10k-random frames of the bench (all 64 the arms
# cycle through, dumped by the bench's own setup), decoded by ours and by
# libzstd: each frame alone, then in turn with the output at several offsets
# of a page, i686 build. Usage: probe.sh
set -eu
echo "cpu $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
echo "family $(grep -m1 'cpu family' /proc/cpuinfo | cut -d: -f2-) model $(grep -m1 '^model[[:space:]]*:' /proc/cpuinfo | cut -d: -f2-)"
target=i686-unknown-linux-gnu
dumps="$PWD/dumps"
mkdir -p "$dumps"
STRUCTURED_ZSTD_DUMP_DICT_DIR="$dumps" STRUCTURED_ZSTD_BENCH_LEVEL_FILTER=level_11_lazy,level_12_lazy,level_13_lazy \
  cargo bench -q -p ffi-bench --bench compare_ffi --target "$target" -- '^nomatch$' > dump.txt 2>&1 || { cat dump.txt; exit 1; }
cargo build -q --profile bench -p ffi-bench --target "$target" --example dict_decode_offsets
bin="target/$target/release/examples/dict_decode_offsets"
for level in level_11_lazy level_12_lazy level_13_lazy; do
  echo "=== $level"
  "$bin" "$dumps/small-10k-random.dict" 10240 20000 "$dumps/small-10k-random.$level" 64
done
