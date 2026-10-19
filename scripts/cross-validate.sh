#!/bin/bash
# Cross-validate our `zstd` against the reference CLI built from the pinned
# upstream release, on silesia, canterbury, the first MiB of enwik8 and the
# decodecorpus fixtures. Every corpus archive is checked against its digest.
#
# Usage: scripts/cross-validate.sh <our-zstd> [cross_validate.py options]
#
# CROSS_VALIDATE_WORK names the directory downloads and builds are kept in
# (default: a fresh temporary directory, removed on exit).
set -euo pipefail

SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
REPO="$(dirname "$SCRIPTS")"
OURS="${1:?usage: $0 <our-zstd> [cross_validate.py options]}"
if [[ ! -f "$OURS" ]]; then
  echo "no such file: $OURS" >&2
  exit 2
fi
OURS="$(cd "$(dirname "$OURS")" && pwd)/$(basename "$OURS")"
shift

# shellcheck source=scripts/upstream-zstd.sh
source "$SCRIPTS/upstream-zstd.sh"

if [[ -n "${CROSS_VALIDATE_WORK:-}" ]]; then
  WORK="$CROSS_VALIDATE_WORK"
  mkdir -p "$WORK"
else
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
fi

UPSTREAM="$(unpack_upstream "$WORK")"
make -C "$UPSTREAM/programs" zstd >/dev/null
REFERENCE="$UPSTREAM/programs/zstd"

CORPORA="$WORK/corpora"
mkdir -p "$CORPORA"
fetch_verified https://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip \
  0626e25f45c0ffb5dc801f13b7c82a3b75743ba07e3a71835a41e3d9f63c77af "$CORPORA/silesia.zip"
fetch_verified https://corpus.canterbury.ac.nz/resources/cantrbry.tar.gz \
  f140e8a5b73d3f53198555a63bfb827889394a42f20825df33c810c3d5e3f8fb "$CORPORA/cantrbry.tar.gz"
fetch_verified https://mattmahoney.net/dc/enwik8.zip \
  547994d9980ebed1288380d652999f38a14fe291a6247c157c3d33d4932534bc "$CORPORA/enwik8.zip"

rm -rf "$CORPORA/silesia" "$CORPORA/canterbury" "$CORPORA/enwiki-1m"
mkdir -p "$CORPORA/silesia" "$CORPORA/canterbury" "$CORPORA/enwiki-1m"
unzip -q "$CORPORA/silesia.zip" -d "$CORPORA/silesia"
tar -xzf "$CORPORA/cantrbry.tar.gz" -C "$CORPORA/canterbury"
unzip -q -o "$CORPORA/enwik8.zip" enwik8 -d "$CORPORA"
head -c 1048576 "$CORPORA/enwik8" > "$CORPORA/enwiki-1m/enwik8-1m"

python3 "$SCRIPTS/cross_validate.py" --ours "$OURS" --reference "$REFERENCE" \
  --corpus "silesia=$CORPORA/silesia" \
  --corpus "canterbury=$CORPORA/canterbury" \
  --corpus "enwiki-1m=$CORPORA/enwiki-1m" \
  --corpus "decodecorpus=$REPO/zstd/decodecorpus_files" \
  "$@"
