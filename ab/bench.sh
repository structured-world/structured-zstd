#!/bin/sh
# The compare_ffi matrix CI runs on main, for this tree (a) and, with
# $CXBR_BASE_CHECKOUT, the base tree (b), on one host. Shards run arm after
# arm, so both arms meet the same stretch of machine state per shard; each
# cell's speed is its paired ratio against libzstd, which cancels drift
# between the arms' runs. Both arms are timed by this tree's harness
# (run-benchmarks.sh). Builds are generic x86-64, as CI and the crate ship;
# the raw skip is off (RAW_SKIP=0, read by the benches at run time), so ours
# searches every block as libzstd does. Prints one line per cell:
#   arm stage scenario level source rust_variant rust_ratio ffi_ratio speed_rust_over_ffi
# then the paired REPORT_PAIR lines.
# SHARDS picks the CI shards (default all): fast-neg fast-dfast lazy-lower
# lazy-upper btopt btultra2. LEVELS (comma-separated) replaces them with one
# run of those levels; FILTER is a criterion regex over the benchmark id.
# [SHARDS="..."] [LEVELS="..."] [FILTER="regex"] ab/bench.sh
set -eu
SHARDS=${SHARDS:-"fast-neg fast-dfast lazy-lower lazy-upper btopt btultra2"}
[ -z "${LEVELS:-}" ] || SHARDS=levels
shard_levels() {
  case $1 in
    levels) echo "$LEVELS" ;;
    fast-neg) echo "level_-7_fast,level_-6_fast,level_-5_fast,level_-4_fast,level_-3_fast,level_-2_fast" ;;
    fast-dfast) echo "level_-1_fast,level_1_fast,level_2_fast,level_3_dfast,level_4_dfast,level_5_greedy" ;;
    lazy-lower) echo "level_6_lazy,level_7_lazy,level_8_lazy,level_9_lazy,level_10_lazy" ;;
    lazy-upper) echo "level_11_lazy,level_12_lazy,level_13_lazy,level_14_lazy,level_15_lazy" ;;
    btopt) echo "level_16_btopt,level_17_btopt,level_1_fast_ldm,level_22_btultra2_ldm,level_1_fast_ldm_dict,level_22_btultra2_ldm_dict" ;;
    btultra2) echo "level_18_btultra,level_19_btultra2,level_20_btultra2,level_21_btultra2,level_22_btultra2" ;;
    *) echo "unknown shard $1" >&2; exit 2 ;;
  esac
}
A="$PWD"
B="${CXBR_BASE_CHECKOUT:-}"
W="$PWD/ab-bench"
trap 'rm -rf "$W" "$A/target-ab" ${B:+"$B/target-ab"}' EXIT
mkdir -p "$W"
TT=x86_64-unknown-linux-gnu
export CARGO_INCREMENTAL=0
RAW_SKIP=0
export RAW_SKIP
echo "host $(hostname) cpus $(nproc) load $(cut -d' ' -f1-3 /proc/loadavg) RAW_SKIP=$RAW_SKIP"
grep -m1 'model name' /proc/cpuinfo
grep -q '"RAW_SKIP"' "$A/zstd/benches/support/mod.rs"

build() {
  # $1 tree, $2 arm
  (cd "$1" && CARGO_TARGET_DIR="$1/target-ab" \
    cargo bench -q --bench compare_ffi -p ffi-bench --target $TT --no-run \
    --message-format=json > "$W/$2.build.json")
  exe=$(jq -r 'select(.executable != null and .target.name == "compare_ffi") | .executable' \
    "$W/$2.build.json" | tail -1)
  cp "$exe" "$W/$2.compare_ffi"
  rm -rf "$1/target-ab" "$W/$2.build.json"
  echo "built $2 ($(git -C "$1" log --oneline -1)) md5 $(md5sum < "$W/$2.compare_ffi" | cut -c1-12)"
  if [ -n "${FILTER:-}" ]; then
    mv "$W/$2.compare_ffi" "$W/$2.compare_ffi.real"
    printf '#!/bin/sh\nexec "%s" "$@" '"'"'%s'"'"'\n' "$W/$2.compare_ffi.real" "$FILTER" \
      > "$W/$2.compare_ffi"
    chmod +x "$W/$2.compare_ffi"
  fi
}
build "$A" a
arms=a
if [ -n "$B" ]; then
  grep -q '"RAW_SKIP"' "$B/zstd/benches/support/mod.rs"
  build "$B" b
  arms="a b"
fi

[ -z "${CXBR_BENCHMARK_PHASE:-}" ] || "$CXBR_BENCHMARK_PHASE" begin
for name in $SHARDS; do
  shard=$(shard_levels "$name")
  for arm in $arms; do
    echo "=== $arm $shard (load $(cut -d' ' -f1 /proc/loadavg))"
    work="$W/run-$arm"
    rm -rf "$work"
    mkdir -p "$work"
    (
      cd "$work"
      STRUCTURED_ZSTD_BENCH_LEVEL_FILTER="$shard" \
        STRUCTURED_ZSTD_BENCH_BIN="$W/$arm.compare_ffi" \
        STRUCTURED_ZSTD_BENCH_CORPUS_PATH="$A/zstd/decodecorpus_files/z000033" \
        STRUCTURED_ZSTD_BENCH_LARGE_BYTES=16777216 \
        bash "$A/.github/scripts/run-benchmarks.sh" > run.log 2>&1 \
        || { tail -40 run.log; exit 1; }
      jq -r --arg t "$arm" '.[] | [$t, .params.stage, .scenario, .params.level,
          (.params.source // "-"), .rust_variant, (.ratio.rust // "-"), (.ratio.ffi // "-"),
          (.speed.delta_rust_over_ffi // "-")] | map(tostring) | join(" ")' benchmark-delta.json
      awk '/^REPORT_PAIR/ {print t, $0}' t="$arm" run.log
    )
    rm -rf "$work"
  done
done
[ -z "${CXBR_BENCHMARK_PHASE:-}" ] || "$CXBR_BENCHMARK_PHASE" end
