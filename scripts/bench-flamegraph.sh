#!/bin/bash
set -euo pipefail

BENCH_FILTER="${1:-compress/default/large-log-stream/matrix/pure_rust}"
OUTPUT_DIR="${2:-target/flamegraph}"

mkdir -p "$OUTPUT_DIR"

echo "Generating flamegraph for benchmark filter: $BENCH_FILTER" >&2
echo "Output directory: $OUTPUT_DIR" >&2

# Use BENCH_FLAMEGRAPH_USE_ROOT=1 to opt into running cargo flamegraph with --root.
EXTRA_FLAMEGRAPH_ARGS=()
if [[ "${BENCH_FLAMEGRAPH_USE_ROOT:-}" == "1" ]]; then
  EXTRA_FLAMEGRAPH_ARGS+=(--root)
fi

if cargo flamegraph \
  --bench compare_ffi \
  -p structured-zstd \
  ${EXTRA_FLAMEGRAPH_ARGS[@]+"${EXTRA_FLAMEGRAPH_ARGS[@]}"} \
  --output "$OUTPUT_DIR/${BENCH_FILTER//\//_}.svg" \
  -- \
  "$BENCH_FILTER"; then
  :
else
  status=$?
  if [[ "${BENCH_FLAMEGRAPH_USE_ROOT:-}" != "1" ]]; then
    cat >&2 <<'EOF'
cargo flamegraph failed. This may be due to insufficient permissions for perf.

If you see a "Permission denied" or "not allowed to access CPU" error, try re-running with:

  BENCH_FLAMEGRAPH_USE_ROOT=1 sudo -E scripts/bench-flamegraph.sh "<bench_filter>" "<output_dir>"

or otherwise ensure perf has sufficient permissions.
EOF
  fi
  exit "$status"
fi
