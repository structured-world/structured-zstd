#!/bin/sh
# Decoding a tiny frame, ours against libzstd, under musl and glibc on one
# host: the dashboard's `decompress / level_-7_fast / small-4k-log-lines`
# shape (one reused decoder and DCtx, `decode_all` into a slice with
# WILDCOPY_OVERLENGTH slack) on the fixture's first frame, 4 KiB of log lines.
# With $CXBR_BASE_CHECKOUT, ours is built from both trees (a = this, b = base)
# and the rounds interleave a, b and libzstd; z000033 at level 3 under glibc is
# the control. Five rounds of counters, then a cycles profile of each side
# under musl and, with LBR, the callers of musl's memcpy in ours.
# Needs musl-gcc, which the dispatcher's `musl` tag declares.
set -eu

echo "host: $(hostname)"
lscpu | head -14
command -v musl-gcc
A="$PWD"
B="${CXBR_BASE_CHECKOUT:-}"
W="$PWD/musl-tiny"
trap 'rm -rf "$W" "$A/target-mt" ${B:+"$B/target-mt"}' EXIT
mkdir -p "$W"

# The scenario's stream: four log lines repeated, the first 4096 bytes.
: > "$W/lines.txt"
i=0
while [ "$i" -lt 12 ]; do
  printf '%s\n' \
    'ts=2026-03-26T21:39:28Z level=INFO msg="flush memtable" tenant=demo table=orders region=eu-west' \
    'ts=2026-03-26T21:39:29Z level=INFO msg="rotate segment" tenant=demo table=orders region=eu-west' \
    'ts=2026-03-26T21:39:30Z level=INFO msg="compact level" tenant=demo table=orders region=eu-west' \
    'ts=2026-03-26T21:39:31Z level=INFO msg="write block" tenant=demo table=orders region=eu-west' \
    >> "$W/lines.txt"
  i=$((i + 1))
done
head -c 4096 "$W/lines.txt" > "$W/log4k.bin"
cp "$A/zstd/decodecorpus_files/z000033" "$W/z33.bin"

rustup target add x86_64-unknown-linux-musl
build() {
  # $1 tree, $2 arm tag
  for libc in musl gnu; do
    tgt=x86_64-unknown-linux-$libc
    (cd "$1" && CARGO_TARGET_DIR="$1/target-mt" CC_x86_64_unknown_linux_musl=musl-gcc \
      CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-cpu=x86-64-v3" \
      CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3" \
      cargo build -q --profile bench -p ffi-bench \
      --example decode_loop_z000033 --example c_decode_loop_z000033 --target "$tgt")
    cp "$1/target-mt/$tgt/release/examples/decode_loop_z000033" "$W/ours-$2-$libc"
    cp "$1/target-mt/$tgt/release/examples/c_decode_loop_z000033" "$W/c-$libc"
  done
}
build "$A" a
arms=a
if [ -n "$B" ]; then
  build "$B" b
  arms="a b"
fi

EV=task-clock,cycles:u,instructions:u,branch-misses:u
ITERS=2000000
run() {
  # $1 label, then the command
  label=$1
  shift
  echo "=== $label"
  perf stat -e "$EV" "$@" 2>&1 | grep -E "ns/iter|task-clock|cycles|instructions|branch-misses"
}
# A host without the benchmark role has no phase controller.
[ -z "${CXBR_BENCHMARK_PHASE:-}" ] || "$CXBR_BENCHMARK_PHASE" begin
for round in 1 2 3 4 5; do
  for libc in musl gnu; do
    for arm in $arms; do
      run "log4k $libc round $round arm $arm" "$W/ours-$arm-$libc" -7 "$ITERS" rust "$W/log4k.bin"
    done
    run "log4k $libc round $round c" "$W/c-$libc" -7 "$ITERS" "$W/log4k.bin"
  done
  for arm in $arms; do
    run "z33-L3 gnu round $round arm $arm" "$W/ours-$arm-gnu" 3 300 rust "$W/z33.bin"
  done
done
[ -z "${CXBR_BENCHMARK_PHASE:-}" ] || "$CXBR_BENCHMARK_PHASE" end

for side in ours-a c; do
  if [ "$side" = c ]; then args="-7 $ITERS $W/log4k.bin"; else args="-7 $ITERS rust $W/log4k.bin"; fi
  echo "=== profile $side musl"
  # shellcheck disable=SC2086
  perf record -q -e cycles:u -F 4999 -o "$W/p-$side.data" "$W/$side-musl" $args > /dev/null 2>&1
  perf report --stdio --no-children --percent-limit 0.5 -i "$W/p-$side.data" -F overhead,sym 2>/dev/null \
    | grep -v "^#" | grep -v "^$" | head -30
done
if perf record -q --user-callchains --call-graph lbr -e cycles:u -F 4999 -o "$W/cg.data" \
  "$W/ours-a-musl" -7 "$ITERS" rust "$W/log4k.bin" > /dev/null 2>&1; then
  echo "=== callers of memcpy (ours musl)"
  perf report --stdio --no-children -S memcpy -G -i "$W/cg.data" 2>/dev/null \
    | grep -v "^#" | grep -v "^$" | head -60
fi
