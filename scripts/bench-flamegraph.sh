#!/bin/bash
set -euo pipefail

BENCH_FILTER="${1:-compress/default/large-log-stream/matrix/pure_rust}"
OUTPUT_DIR="${2:-target/flamegraph}"

mkdir -p "$OUTPUT_DIR"

echo "Generating flamegraph for benchmark filter: $BENCH_FILTER" >&2
echo "Output directory: $OUTPUT_DIR" >&2

cargo flamegraph \
  --bench compare_ffi \
  -p structured-zstd \
  --root \
  --output "$OUTPUT_DIR/${BENCH_FILTER//\//_}.svg" \
  -- \
  --bench \
  "$BENCH_FILTER"
