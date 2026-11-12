#!/usr/bin/env bash
# Builds the shared initramfs: a gzip newc cpio whose only file is /init, the
# static musl dassie-init (ADR 0006).
#
#   guest/initramfs.sh [x86_64|aarch64]
#
# Outputs $OUT/initramfs.cpio.gz. Needs: cargo, GNU cpio, gzip.
#
# No device nodes are packed, so building needs no root. The kernel's built-in
# initramfs already holds /dev/console, which the external one is unpacked
# over, and init mounts devtmpfs before anything else.

# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

log "building dassie-init for $TRIPLE"
cargo_musl -p dassie-init --bin dassie-init

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging"/{dev,proc,sys,newroot}
install -m 0755 "$MUSL_RELEASE/dassie-init" "$staging/init"
find "$staging" -exec touch -h -d @0 {} +

(cd "$staging" && find . -mindepth 1 | LC_ALL=C sort |
  cpio --quiet -o -H newc -R 0:0 --reproducible) | gzip -9n >"$OUT/initramfs.cpio.gz"
log "wrote $OUT/initramfs.cpio.gz"
