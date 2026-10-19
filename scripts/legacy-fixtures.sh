#!/usr/bin/env bash
# Regenerate zstd/legacy_frames: frames written by the v0.5, v0.6 and v0.7
# release command lines, which the decoder must read back byte-exact.
#
# Usage: scripts/legacy-fixtures.sh [WORK_DIR]
#
# Each release is fetched from its tag, checked against a pinned digest and
# built from source. The inputs are files already in the repository, so every
# frame decodes to a file the tests can compare against.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/upstream-zstd.sh
source "$root/scripts/upstream-zstd.sh"

work="${1:-$(mktemp -d)}"
mkdir -p "$work"
out="$root/zstd/legacy_frames"
corpus="$root/zstd/decodecorpus_files"
samples="$root/zstd/dict_tests/files"

# version tag, tarball digest
releases=(
  "0.5.1 c23be381911764056087a6d0280544589f2fe44daef4c9223fe08ede6def45dc"
  "0.6.2 f80c35b2860d66299c8457b891d05277ce4e88b9bdd1f10a431e74804404cb8f"
  "0.7.5 6800defac9a93ddb1d9673d62c78526800df71cf9f353456f8e488ba4de51061"
)

# Shapes the block decoder takes different paths for: empty, one byte, a
# few bytes, short text, several blocks, data that stays raw, and a mixed
# file with long matches and four-stream literals.
inputs=(z000062 z000013 z000001 abc.txt z000000 z000046 z000021)

# build <version> <digest>: the release's command line; prints its path.
build() {
  local version="$1" digest="$2" dir="$work/v$1"
  mkdir -p "$dir"
  fetch_verified "https://github.com/facebook/zstd/archive/refs/tags/v${version}.tar.gz" \
    "$digest" "$dir/zstd-${version}.tar.gz"
  rm -rf "$dir/zstd-${version}"
  tar -xzf "$dir/zstd-${version}.tar.gz" -C "$dir"
  make -s -C "$dir/zstd-${version}/programs" zstd MOREFLAGS=-Wno-error >&2
  echo "$dir/zstd-${version}/programs/zstd"
}

rm -rf "$out"
mkdir -p "$out"

# A frame several windows long, whose matches reach back across the point
# where a buffered decoder starts its next segment: z000000 eight times, each
# copy a match on the one before. Tests rebuild it from the name `<file>x<N>`.
repeated="$work/z000000x8"
: >"$repeated"
for _ in 1 2 3 4 5 6 7 8; do
  cat "$corpus/z000000" >>"$repeated"
done

for release in "${releases[@]}"; do
  read -r version digest <<<"$release"
  tag="v0${version:2:1}"
  zstd_bin="$(build "$version" "$digest")"

  for input in "${inputs[@]}"; do
    for level in 1 19; do
      "$zstd_bin" -q -f "-$level" "$corpus/$input" -o "$out/$input.$tag.l$level.zst"
    done
  done
  "$zstd_bin" -q -f -1 "$repeated" -o "$out/z000000x8.$tag.l1.zst"

  # A dictionary trained by the release itself, and frames that need it.
  # The training set is listed in sorted order, so the dictionary is the same
  # on every run; v0.7 would pick a random ID, so it gets a fixed one.
  dict_args=()
  if [[ "$tag" == v07 ]]; then
    dict_args=(--dictID 7)
  fi
  find "$samples" -type f | LC_ALL=C sort | head -n 200 >"$work/samples.$tag"
  # shellcheck disable=SC2046
  "$zstd_bin" -q -f --train $(cat "$work/samples.$tag") --maxdict 16384 \
    "${dict_args[@]}" -o "$out/dictionary.$tag"
  for sample in $(LC_ALL=C ls "$samples" | head -n 3); do
    "$zstd_bin" -q -f -3 -D "$out/dictionary.$tag" "$samples/$sample" \
      -o "$out/$sample.$tag.dict.zst"
  done

  if [[ "$tag" == v07 ]]; then
    # The content checksum and the dictionary ID are optional in v0.7.
    "$zstd_bin" -q -f -3 --no-check "$corpus/z000000" -o "$out/z000000.$tag.nocheck.zst"
    sample="$(LC_ALL=C ls "$samples" | head -n 1)"
    "$zstd_bin" -q -f -3 --no-dictID -D "$out/dictionary.$tag" "$samples/$sample" \
      -o "$out/$sample.$tag.dict-noid.zst"
  fi
done

echo "wrote $(find "$out" -type f | wc -l | tr -d ' ') files to $out"
