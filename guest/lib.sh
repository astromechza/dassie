# shellcheck shell=bash
# Shared helpers for the guest asset scripts. Source it; do not run it.
#
# Sets:
#   REPO       repo root
#   ARCH       x86_64 | aarch64 (from $1 or the host)
#   TRIPLE     the musl Rust target for ARCH
#   OUT        output dir for ARCH, default target/guest/$ARCH
#   CACHE_DIR  downloads cache, default ~/.cache/dassie

set -euo pipefail
# Keep errexit inside $(...), so a failure in `fetch` stops the caller.
shopt -s inherit_errexit

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ARCH=${1:-$(uname -m)}
case "$ARCH" in
  x86_64 | aarch64) ;;
  arm64) ARCH=aarch64 ;;
  *) echo "unsupported arch: $ARCH (want x86_64 or aarch64)" >&2; exit 2 ;;
esac
TRIPLE="$ARCH-unknown-linux-musl"
OUT=${OUT:-$REPO/target/guest/$ARCH}
CACHE_DIR=${CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/dassie}
mkdir -p "$OUT" "$CACHE_DIR"

log() { echo "==> $*" >&2; }

# fetch URL SHA256 -> prints the cached path, downloading and verifying once.
# Fails, printing nothing, if the download or the checksum fails.
fetch() {
  local url=$1 sha=$2 dest
  dest="$CACHE_DIR/$(basename "$url")"
  if [[ ! -f "$dest" ]] || ! echo "$sha  $dest" | sha256sum -c --quiet - 2>/dev/null; then
    log "downloading $url"
    curl -fsSL --retry 3 -o "$dest.tmp" "$url" || return 1
    if ! echo "$sha  $dest.tmp" | sha256sum -c --quiet -; then
      rm -f "$dest.tmp"
      echo "checksum mismatch for $url" >&2
      return 1
    fi
    mv "$dest.tmp" "$dest" || return 1
  fi
  echo "$dest"
}

# cargo_musl ARGS... builds release artifacts for TRIPLE.
cargo_musl() {
  cargo build --locked --release --target "$TRIPLE" "$@" --manifest-path "$REPO/Cargo.toml"
}

# Where cargo_musl puts binaries.
# shellcheck disable=SC2034
MUSL_RELEASE="$REPO/target/$TRIPLE/release"
