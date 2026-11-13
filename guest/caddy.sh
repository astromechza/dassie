#!/usr/bin/env bash
# Downloads the pinned Caddy release for the host's ingress (ADR 0010) and
# verifies its checksum.
#
#   guest/caddy.sh [x86_64|aarch64]
#
# Outputs $OUT/caddy. Install it as /usr/local/bin/caddy, for
# systemd/caddy.service.

# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

# The sha256 of each tarball. Caddy publishes sha512s in
# caddy_<version>_checksums.txt; check a new pin against those too.
CADDY_VERSION=2.11.9
case "$ARCH" in
  x86_64)
    GOARCH=amd64
    CADDY_SHA256=5b8cb7faf8726a2028ad96a66e54e6e81e8f361bc6175e5d22bbd997cc5177fb
    ;;
  aarch64)
    GOARCH=arm64
    CADDY_SHA256=5e377cff994fc9362a6fcf695c53f226cfca3a3ef85bd3a3687cf252d6310165
    ;;
esac

name="caddy_${CADDY_VERSION}_linux_$GOARCH.tar.gz"
tgz=$(fetch "https://github.com/caddyserver/caddy/releases/download/v$CADDY_VERSION/$name" "$CADDY_SHA256")
tar -xzf "$tgz" -C "$OUT" caddy
log "wrote $OUT/caddy ($("$OUT/caddy" version 2>/dev/null || echo "v$CADDY_VERSION, $ARCH"))"
