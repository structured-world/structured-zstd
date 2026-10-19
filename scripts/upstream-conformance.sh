#!/bin/bash
# Run upstream zstd's own command-line test suites against our `zstd`:
# tests/playTests.sh and tests/cli-tests, from the pinned release tarball.
#
# Usage: scripts/upstream-conformance.sh <path-to-our-zstd> [--test-large-data]
#
# CONFORMANCE_WORK names the directory the tarball is unpacked and the suites
# run in (default: a fresh temporary directory, removed on success).
set -euo pipefail

ZSTD_BIN="$(cd "$(dirname "${1:?usage: $0 <zstd-binary> [--test-large-data]}")" && pwd)/$(basename "$1")"
shift
LARGE_DATA="${1:-}"

# shellcheck source=scripts/upstream-zstd.sh
source "$(dirname "$0")/upstream-zstd.sh"

if [[ -n "${CONFORMANCE_WORK:-}" ]]; then
  WORK="$CONFORMANCE_WORK"
  mkdir -p "$WORK"
  KEEP_WORK=1
else
  WORK="$(mktemp -d)"
  KEEP_WORK=0
fi

UPSTREAM="$(unpack_upstream "$WORK")"
make -C "$UPSTREAM/tests" datagen >/dev/null
DATAGEN_BIN="$UPSTREAM/tests/datagen"

echo "== playTests.sh ${LARGE_DATA}"
mkdir -p "$WORK/playtests"
(
  cd "$WORK/playtests"
  # The variable upstream's Makefile passes: 0 runs the scenarios that need
  # the double-fast and stronger compressors, which this build has.
  ZSTD_LIB_EXCLUDE_COMPRESSORS_DFAST_AND_UP=0 \
    ZSTD_BIN="$ZSTD_BIN" DATAGEN_BIN="$DATAGEN_BIN" \
    sh "$UPSTREAM/tests/playTests.sh" ${LARGE_DATA:+"$LARGE_DATA"}
)

echo "== cli-tests"
CLI_TESTS="$UPSTREAM/tests/cli-tests"
# Cases this build cannot run, each with the reason; everything else runs.
#
# file-stat/: `--trace-file-stat` prints a trace of the reference's internal
#   file-system helpers by their C names (UTIL_stat, UTIL_isLink, ...), so the
#   expected output describes its implementation rather than any behaviour.
# compression/adapt.sh, compression/multi-threaded.sh: they need a
#   multi-threaded build (adaptive levels, the `-T` decompression warning);
#   upstream's own single-threaded build fails them the same way.
# basic/version.sh, progress/: they expect the reference's own banner
#   (`*** Zstandard CLI ... by Yann Collet ***`) and progress wording; this
#   command names itself and draws its own progress.
rm -rf "$CLI_TESTS/file-stat" "$CLI_TESTS/progress"
rm -f "$CLI_TESTS/basic/version.sh"*
rm -f "$CLI_TESTS/compression/adapt.sh"* "$CLI_TESTS/compression/multi-threaded.sh"*
python3 "$CLI_TESTS/run.py" --zstd "$ZSTD_BIN" --datagen "$DATAGEN_BIN"

if [[ "$KEEP_WORK" == 0 ]]; then
  rm -rf "$WORK"
fi
echo "== upstream suites passed"
