#!/usr/bin/env bash
# Boots the test VM twice and checks the lifecycle events: the entrypoint is
# told to exit after a few seconds, so the listener must see `started`,
# `ready` and `exited` in that order, and Firecracker must exit when the
# guest reboots. The non-root testapp must also have been able to use
# /dev/stderr and /dev/shm.
#
# Both boots use the built, read-only rootfs itself, as every VM of an image
# shares its artefact. testapp writes a file to the overlay root at startup
# and fails if one is already there, so the second boot proves the first
# boot's writes were discarded. The rootfs must be byte-for-byte unchanged
# afterwards (ADR 0006).
#
#   scripts/test/smoke-test.sh [x86_64|aarch64]
#
# Expects the outputs of kernel/build.sh, initramfs.sh, rootfs.sh and
# firecracker.sh in $OUT, and read-write access to /dev/kvm.

# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

case "$ARCH" in
  x86_64) kernel="$OUT/vmlinux" ;;
  aarch64) kernel="$OUT/Image" ;;
esac
timeout_secs=${SMOKE_TIMEOUT:-60}

cargo build --locked -p dassie-proto --example listener --manifest-path "$REPO/Cargo.toml"
listener="$REPO/target/debug/examples/listener"

rootfs="$OUT/rootfs.erofs"
before=$(sha256sum "$rootfs")

# boot N: boots the VM once, with its files in $OUT/smoke/N.
boot() {
  local run="$OUT/smoke/$1"
  rm -rf "$run"
  mkdir -p "$run"
  local uds="$run/vm.vsock"
  "$REPO/scripts/host/vm-config.sh" "$kernel" "$OUT/initramfs.cpio.gz" "$rootfs" "$uds" \
    "dassie.instance=smoke-$1 dassie.env=EXIT_AFTER=3" >"$run/config.json"

  "$listener" events "$uds" --until exited --timeout "$timeout_secs" \
    >"$run/events.log" 2>"$run/listener.log" &
  local listener_pid=$!
  # The guest holds its events until the host listens, but start in order anyway.
  for _ in $(seq 50); do
    [[ -S "${uds}_1024" ]] && break
    sleep 0.1
  done

  log "booting $ARCH VM ($1)"
  timeout "$((timeout_secs + 10))" "$OUT/firecracker" --no-api --config-file "$run/config.json" \
    >"$run/console.log" 2>&1 &
  local fc_pid=$!

  local status=0
  wait "$listener_pid" || status=$?
  # Firecracker exits once the guest reboots after `exited`.
  local fc_status=0
  wait "$fc_pid" || fc_status=$?

  dump() {
    echo "--- console ---" && cat "$run/console.log"
    echo "--- listener ---" && cat "$run/listener.log"
    echo "--- events ---" && cat "$run/events.log"
  }

  if ((status != 0)); then
    dump
    echo "FAIL: boot $1: listener saw no exited event (status $status)" >&2
    exit 1
  fi
  if ! awk '/Started/ { s = NR } /Ready/ && s { r = NR } /Exited/ && r { ok = 1 } END { exit !ok }' "$run/events.log"; then
    dump
    echo "FAIL: boot $1: events not in started, ready, exited order" >&2
    exit 1
  fi
  # testapp runs as a non-root user and exits early unless it can reopen
  # /dev/stderr and write /dev/shm (ADR 0005, ADR 0006), and unless the
  # overlay root is 0755 root and holds no file from an earlier boot.
  local want
  for want in "testapp: /dev/stderr ok" "testapp: /dev/shm ok" \
    "testapp: / is 0755 root" "testapp: clean root, write ok"; do
    if ! grep -qF "$want" "$run/console.log"; then
      dump
      echo "FAIL: boot $1: console has no \"$want\"" >&2
      exit 1
    fi
  done
  if ((fc_status != 0)); then
    dump
    echo "FAIL: boot $1: firecracker exited with $fc_status (124 means it never exited)" >&2
    exit 1
  fi
  cat "$run/events.log"
}

boot 1
boot 2

if [[ "$(sha256sum "$rootfs")" != "$before" ]]; then
  echo "FAIL: the read-only rootfs changed: $before" >&2
  exit 1
fi
log "PASS: two boots of one read-only rootfs: started, ready, exited; non-root /dev ok;" \
  "clean overlay root each time; rootfs unchanged; firecracker exited cleanly"
