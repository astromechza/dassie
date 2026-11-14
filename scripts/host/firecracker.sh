#!/usr/bin/env bash
# Downloads the pinned Firecracker release and verifies its checksum.
#
#   scripts/host/firecracker.sh [x86_64|aarch64]
#
# Outputs $OUT/firecracker and $OUT/jailer (ADR 0013), from the same release.

# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

FC_VERSION=v1.17.0
case "$ARCH" in
  x86_64) FC_SHA256=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558 ;;
  aarch64) FC_SHA256=e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256 ;;
esac

name="firecracker-$FC_VERSION-$ARCH"
tgz=$(fetch "https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VERSION/$name.tgz" "$FC_SHA256")
jailer="jailer-$FC_VERSION-$ARCH"
tar -xzf "$tgz" -C "$OUT" --strip-components=1 "release-$FC_VERSION-$ARCH/$name" \
  "release-$FC_VERSION-$ARCH/$jailer"
mv "$OUT/$name" "$OUT/firecracker"
mv "$OUT/$jailer" "$OUT/jailer"
log "wrote $OUT/firecracker ($("$OUT/firecracker" --version | head -1))"
log "wrote $OUT/jailer ($("$OUT/jailer" --version | head -1))"
