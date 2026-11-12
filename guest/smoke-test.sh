#!/usr/bin/env bash
# Boots the test VM once and checks the lifecycle events: the entrypoint is
# told to exit after a few seconds, so the listener must see `started`,
# `ready` and `exited` in that order, and Firecracker must exit when the
# guest reboots.
#
#   guest/smoke-test.sh [x86_64|aarch64]
#
# Expects the outputs of kernel/build.sh, initramfs.sh, rootfs.sh and
# firecracker.sh in $OUT, and read-write access to /dev/kvm.

# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

case "$ARCH" in
  x86_64) kernel="$OUT/vmlinux" ;;
  aarch64) kernel="$OUT/Image" ;;
esac
timeout_secs=${SMOKE_TIMEOUT:-60}

cargo build --locked -p dassie-proto --example listener --manifest-path "$REPO/Cargo.toml"
listener="$REPO/target/debug/examples/listener"

run="$OUT/smoke"
rm -rf "$run"
mkdir -p "$run"
uds="$run/vm.vsock"
# Firecracker writes its rootfs; keep the built image pristine.
cp "$OUT/rootfs.ext4" "$run/rootfs.ext4"
"$REPO/guest/vm-config.sh" "$kernel" "$OUT/initramfs.cpio.gz" "$run/rootfs.ext4" "$uds" \
  "dassie.instance=smoke dassie.env=EXIT_AFTER=3" >"$run/config.json"

"$listener" events "$uds" --until exited --timeout "$timeout_secs" >"$run/events.log" 2>"$run/listener.log" &
listener_pid=$!
# The guest holds its events until the host listens, but start in order anyway.
for _ in $(seq 50); do
  [[ -S "${uds}_1024" ]] && break
  sleep 0.1
done

log "booting $ARCH VM"
timeout "$((timeout_secs + 10))" "$OUT/firecracker" --no-api --config-file "$run/config.json" \
  >"$run/console.log" 2>&1 &
fc_pid=$!

status=0
wait "$listener_pid" || status=$?
# Firecracker exits once the guest reboots after `exited`.
fc_status=0
wait "$fc_pid" || fc_status=$?

dump() {
  echo "--- console ---" && cat "$run/console.log"
  echo "--- listener ---" && cat "$run/listener.log"
  echo "--- events ---" && cat "$run/events.log"
}

if ((status != 0)); then
  dump
  echo "FAIL: listener saw no exited event (status $status)" >&2
  exit 1
fi
if ! awk '/Started/ { s = NR } /Ready/ && s { r = NR } /Exited/ && r { ok = 1 } END { exit !ok }' "$run/events.log"; then
  dump
  echo "FAIL: events not in started, ready, exited order" >&2
  exit 1
fi
if ((fc_status != 0)); then
  dump
  echo "FAIL: firecracker exited with $fc_status (124 means it never exited)" >&2
  exit 1
fi
cat "$run/events.log"
log "PASS: started, ready, exited; firecracker exited cleanly"
