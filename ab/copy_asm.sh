#!/bin/sh
# Where the cycles of a tiny frame's long last match go under glibc, ours (a,
# this tree) against base (b, $CXBR_BASE_CHECKOUT): the hot instructions of
# each side's copy (`perf annotate` of every symbol over 2%), then store and
# load counters that tell split, blocked or buffer-bound stores apart. Same
# fixture and harness as `musl_tiny.sh`. Needs the base checkout and a PMU.
set -eu

echo "host: $(hostname)"
lscpu | grep -E "Model name|Flags" | cut -c1-400
A="$PWD"
B="$CXBR_BASE_CHECKOUT"
W="$PWD/copy-asm"
trap 'rm -rf "$W" "$A/target-ca" "$B/target-ca"' EXIT
mkdir -p "$W"

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

build() {
  # $1 tree, $2 arm tag
  # Generic x86-64, as CI and the crate ship.
  (cd "$1" && CARGO_TARGET_DIR="$1/target-ca" \
    cargo build -q --profile bench -p ffi-bench --example decode_loop_z000033 \
    --target x86_64-unknown-linux-gnu)
  cp "$1/target-ca/x86_64-unknown-linux-gnu/release/examples/decode_loop_z000033" "$W/ours-$2"
}
build "$A" a
build "$B" b
ldd "$W/ours-b" | grep libc
ITERS=2000000

for arm in a b; do
  echo "=== profile arm $arm"
  perf record -q -e cycles:u -F 4999 -o "$W/p-$arm.data" "$W/ours-$arm" -7 "$ITERS" rust "$W/log4k.bin" > /dev/null 2>&1
  perf report --stdio --no-children --percent-limit 1 -i "$W/p-$arm.data" -F overhead,dso,sym 2>/dev/null \
    | grep -v "^#" | grep -v "^$" | head -20
  echo "=== annotate arm $arm (instructions with >= 0.4% of the symbol)"
  perf annotate --stdio --percent-limit 2 -i "$W/p-$arm.data" 2>/dev/null \
    | awk '/Percent \|/ { print; next } /^ *[0-9]+\.[0-9]+ *:/ { if ($1 + 0 >= 0.4) print; next }' \
    | head -400
done

for arm in a b; do
  echo "=== branch-miss sites arm $arm"
  perf record -q -e branch-misses:u -c 1009 -o "$W/bm-$arm.data" "$W/ours-$arm" -7 "$ITERS" rust "$W/log4k.bin" > /dev/null 2>&1
  perf annotate --stdio --percent-limit 5 -i "$W/bm-$arm.data" 2>/dev/null \
    | awk '/Percent \|/ { print; next } /^ *[0-9]+\.[0-9]+ *:/ { if ($1 + 0 >= 2) print; next }' \
    | head -80
done

# Broadwell names; an event the PMU lacks prints its own error and the next runs.
for ev in \
  cycles:u,instructions:u \
  mem_uops_retired.split_stores:u,mem_uops_retired.split_loads:u \
  mem_uops_retired.all_stores:u,mem_uops_retired.all_loads:u \
  ld_blocks.store_forward:u,ld_blocks_partial.address_alias:u \
  resource_stalls.sb:u,cycle_activity.stalls_mem_any:u \
  offcore_requests.demand_rfo:u,l2_rqsts.all_rfo:u; do
  for round in 1 2; do
    for arm in a b; do
      echo "=== $ev round $round arm $arm"
      perf stat -x, -e "$ev" "$W/ours-$arm" -7 "$ITERS" rust "$W/log4k.bin" 2>&1 >/dev/null \
        | grep -v "^decoded" || echo "unavailable: $ev"
    done
  done
done
