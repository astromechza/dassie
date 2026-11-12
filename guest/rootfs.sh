#!/usr/bin/env bash
# Builds the test app rootfs: an ext4 image holding the `testapp` example as
# the entrypoint, its OCI config at /.dassie/image.json, and /etc/passwd and
# /etc/group for its `app` user (ADR 0005).
#
#   guest/rootfs.sh [x86_64|aarch64]
#
# Outputs $OUT/rootfs.ext4. Needs: cargo, mkfs.ext4 (e2fsprogs) with -d.
# Runs without root: `mkfs.ext4 -d` copies a staging directory. Files keep the
# building user's uid, which the guest does not care about.

# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

case "$ARCH" in
  x86_64) oci_arch=amd64 ;;
  aarch64) oci_arch=arm64 ;;
esac

log "building testapp for $TRIPLE"
cargo_musl -p dassie-init --example testapp

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging"/{.dassie,dev,proc,sys,tmp,etc,usr/local/bin,home/app}
chmod 1777 "$staging/tmp"
install -m 0755 "$MUSL_RELEASE/examples/testapp" "$staging/usr/local/bin/testapp"
sed "s/@ARCH@/$oci_arch/" "$REPO/guest/rootfs/image.json" >"$staging/.dassie/image.json"
cat >"$staging/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
app:x:1000:1000:app:/home/app:/bin/sh
EOF
cat >"$staging/etc/group" <<'EOF'
root:x:0:
app:x:1000:
EOF

rm -f "$OUT/rootfs.ext4"
mkfs.ext4 -q -F -L dassie-test -E root_owner=0:0 -d "$staging" "$OUT/rootfs.ext4" 64M
log "wrote $OUT/rootfs.ext4"
