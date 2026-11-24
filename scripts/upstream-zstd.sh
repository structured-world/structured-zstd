# shellcheck shell=bash
# Sourced by the conformance scripts: the pinned upstream zstd release and
# a verified download helper.

UPSTREAM_VERSION=1.6.0
# The digest upstream publishes next to the tarball (`zstd-1.6.0.tar.gz.sha256`).
UPSTREAM_TARBALL_SHA256=2ba599ae2a257976278e7e71e998433a387bfd18aa7b78678229fd5d84f5e9b8
UPSTREAM_TARBALL_URL="https://github.com/facebook/zstd/releases/download/v${UPSTREAM_VERSION}/zstd-${UPSTREAM_VERSION}.tar.gz"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch_verified <url> <sha256> <destination>: download unless already
# present, then refuse a file whose digest differs.
fetch_verified() {
  if [[ ! -f "$3" ]]; then
    curl --retry 5 --retry-all-errors --retry-delay 2 -fsSL "$1" -o "$3"
  fi
  local actual
  actual="$(sha256_of "$3")"
  if [[ "$actual" != "$2" ]]; then
    echo "error: $3 has digest $actual, expected $2" >&2
    # Gone, so a kept work directory downloads it afresh next time.
    rm -f "$3"
    exit 1
  fi
}

# unpack_upstream <dir>: a fresh source tree of the pinned release under
# <dir>; prints its path.
unpack_upstream() {
  local tarball="$1/zstd-${UPSTREAM_VERSION}.tar.gz"
  fetch_verified "$UPSTREAM_TARBALL_URL" "$UPSTREAM_TARBALL_SHA256" "$tarball"
  rm -rf "$1/zstd-${UPSTREAM_VERSION}"
  tar -xzf "$tarball" -C "$1"
  echo "$1/zstd-${UPSTREAM_VERSION}"
}
