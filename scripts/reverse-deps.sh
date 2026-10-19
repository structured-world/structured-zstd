#!/bin/bash
# Swap this zstd in under real programs built against the reference one and
# check they still work:
#
#   tar      GNU tar --zstd, which runs a `zstd` program: ours creates, the
#            system's extracts, and the other way round.
#   zstdcat  ours, called by that name, at the head of a shell pipeline.
#   bsdtar   libarchive's tar, linked against libzstd: our shared object
#            preloaded in its place, archives crossing to and from the system
#            library.
#   rsync    rsync --compress-choice=zstd, linked against libzstd, with our
#            shared object preloaded on both ends of the transfer.
#
# A preloaded run counts only when the dynamic linker bound the program's
# ZSTD_* calls to our object, so a green result cannot be the system library
# doing the work. Linux only.
#
# Usage: scripts/reverse-deps.sh <our-zstd> <our-libzstd.so>
set -euo pipefail

abs() { echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; }
for input in "${1:?usage: $0 <our-zstd> <our-libzstd.so>}" "${2:?usage: $0 <our-zstd> <our-libzstd.so>}"; do
  if [[ ! -f "$input" ]]; then
    echo "no such file: $input" >&2
    exit 2
  fi
done
ZSTD_BIN="$(abs "$1")"
OUR_LIB="$(abs "$2")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The system's tools stay on the default PATH; ours answer under the reference
# names from their own directory, put first only where ours is meant to run.
mkdir -p "$WORK/bin"
for name in zstd zstdcat unzstd; do
  ln -s "$ZSTD_BIN" "$WORK/bin/$name"
done
OURS_PATH="$WORK/bin:$PATH"

# An input tree with text, repeats, an empty file and incompressible bytes.
mkdir -p "$WORK/src/sub/deeper"
seq 1 200000 > "$WORK/src/numbers.txt"
for i in $(seq 1 300); do echo "line $i of a log that repeats itself"; done > "$WORK/src/sub/log.txt"
head -c 300000 /dev/urandom > "$WORK/src/sub/deeper/random.bin"
: > "$WORK/src/empty"

same_tree() {
  diff -r "$WORK/src" "$1/src"
}

passed=()

echo "== tar --zstd"
PATH="$OURS_PATH" tar --zstd -cf "$WORK/ours.tar.zst" -C "$WORK" src
mkdir "$WORK/tar-system-out"
tar --zstd -xf "$WORK/ours.tar.zst" -C "$WORK/tar-system-out"
same_tree "$WORK/tar-system-out"
tar --zstd -cf "$WORK/system.tar.zst" -C "$WORK" src
mkdir "$WORK/tar-ours-out"
PATH="$OURS_PATH" tar --zstd -xf "$WORK/system.tar.zst" -C "$WORK/tar-ours-out"
same_tree "$WORK/tar-ours-out"
passed+=(tar)

echo "== zstdcat | cat"
# The same frame through the system's zstd and through ours as zstdcat, at the
# head of a pipeline; the outputs must match byte for byte.
zstd -dc "$WORK/system.tar.zst" > "$WORK/system.tar"
"$WORK/bin/zstdcat" "$WORK/system.tar.zst" | cat > "$WORK/zstdcat.tar"
if ! cmp -s "$WORK/system.tar" "$WORK/zstdcat.tar"; then
  echo "zstdcat output differs from the system zstd's" >&2
  exit 1
fi
passed+=(zstdcat)

# Run "$@" with our object preloaded, then require that the dynamic linker
# bound at least one ZSTD_* call to it.
preloaded() {
  local log="$WORK/ld-$1"
  shift
  LD_PRELOAD="$OUR_LIB" LD_DEBUG=bindings LD_DEBUG_OUTPUT="$log" "$@"
  # grep reads the logs itself: behind a pipe, `grep -q` stopping at the first
  # hit kills the writer with SIGPIPE, and pipefail reads that as no match.
  if ! grep -qs "to $OUR_LIB .*symbol \`ZSTD_" "$log".*; then
    echo "no ZSTD_* call was bound to $OUR_LIB" >&2
    exit 1
  fi
}

links_libzstd() {
  local deps
  deps="$(ldd "$(command -v "$1")" 2>/dev/null)" || return 1
  [[ "$deps" == *libzstd.so* ]]
}

echo "== bsdtar --zstd"
if command -v bsdtar >/dev/null && links_libzstd bsdtar; then
  preloaded bsdtar-c bsdtar --zstd -cf "$WORK/ours.bsdtar.zst" -C "$WORK" src
  mkdir "$WORK/bsdtar-system-out"
  bsdtar -xf "$WORK/ours.bsdtar.zst" -C "$WORK/bsdtar-system-out"
  same_tree "$WORK/bsdtar-system-out"
  bsdtar --zstd -cf "$WORK/system.bsdtar.zst" -C "$WORK" src
  mkdir "$WORK/bsdtar-ours-out"
  preloaded bsdtar-x bsdtar -xf "$WORK/system.bsdtar.zst" -C "$WORK/bsdtar-ours-out"
  same_tree "$WORK/bsdtar-ours-out"
  passed+=(bsdtar)
else
  echo "skipped: no bsdtar linked against libzstd"
fi

echo "== rsync --compress-choice=zstd"
if command -v rsync >/dev/null && links_libzstd rsync; then
  # A remote shell that drops the host and runs the far end locally, so both
  # ends of a compressed transfer run here, under the preload.
  printf '#!/bin/sh\nshift\nexec "$@"\n' > "$WORK/rsh"
  chmod +x "$WORK/rsh"
  mkdir "$WORK/rsync-out"
  preloaded rsync rsync -a -z --compress-choice=zstd -e "$WORK/rsh" \
    "$WORK/src" "localhost:$WORK/rsync-out/"
  same_tree "$WORK/rsync-out"
  passed+=(rsync)
else
  echo "skipped: no rsync linked against libzstd"
fi

echo "== reverse dependencies passed: ${passed[*]}"
if (( ${#passed[@]} < 3 )); then
  echo "fewer than three consumers ran" >&2
  exit 1
fi
