#!/usr/bin/env bash
# Builds the test app rootfs: an erofs image holding the `testapp` example as
# the entrypoint, its OCI config at /.dassie/image.json, and /etc/passwd and
# /etc/group for its `app` user (ADR 0005). It is the same format as image
# artefacts (ADR 0009), read-only (0444) like them, and booted the same way:
# as the lower layer of the guest's overlay root (ADR 0006).
#
#   scripts/guest/rootfs.sh [x86_64|aarch64]
#
# Outputs $OUT/rootfs.erofs. Needs: cargo, mkfs.erofs (erofs-utils).
# Runs without root: mkfs.erofs copies a staging directory, and --all-root
# makes every file root-owned, as in an image.

# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

case "$ARCH" in
  x86_64) oci_arch=amd64 ;;
  aarch64) oci_arch=arm64 ;;
esac

log "building testapp for $TRIPLE"
cargo_musl -p dassie-init --example testapp

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging"/{.dassie,dev,proc,sys,tmp,etc,usr/local/bin,home/app}
# mktemp makes it 0700, and mkfs.erofs keeps the source directory's mode.
chmod 0755 "$staging"
chmod 1777 "$staging/tmp"
install -m 0755 "$MUSL_RELEASE/examples/testapp" "$staging/usr/local/bin/testapp"
sed "s/@ARCH@/$oci_arch/" "$REPO/scripts/guest/rootfs/image.json" >"$staging/.dassie/image.json"
cat >"$staging/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
app:x:1000:1000:app:/home/app:/bin/sh
EOF
cat >"$staging/etc/group" <<'EOF'
root:x:0:
app:x:1000:
EOF

rm -f "$OUT/rootfs.erofs"
mkfs.erofs --quiet --all-root -zlz4hc -L dassie-test "$OUT/rootfs.erofs" "$staging"
chmod 0444 "$OUT/rootfs.erofs"
log "wrote $OUT/rootfs.erofs"
