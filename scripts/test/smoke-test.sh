#!/usr/bin/env bash
# Boots the test VM twice through `dassie vm-run` (ADR 0012) and checks the
# lifecycle: the entrypoint is told to exit with code 3 after a few seconds,
# so vm-run must report `started`, `ready` and `exited` in that order and
# exit with 3 itself. The non-root testapp must also have been able to use
# /dev/stderr and /dev/shm.
#
# Boot 1 runs vm-run directly, unjailed. Boot 2 runs it under `systemd-run`
# as a `Type=notify` unit in Firecracker's jailer, as fc@.service does, when
# systemd-run and passwordless sudo are there (as on CI): the unit must then
# reach `ready` through READY=1 and end with result `exit-code` and status
# 3, and the jail must belong to the jail uid (ADR 0013).
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
# firecracker.sh (firecracker and jailer) in $OUT, and read-write access to
# /dev/kvm.

# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

case "$ARCH" in
  x86_64) kernel="$OUT/vmlinux" ;;
  aarch64) kernel="$OUT/Image" ;;
esac
timeout_secs=${SMOKE_TIMEOUT:-60}

cargo build --locked -p dassie --manifest-path "$REPO/Cargo.toml"
dassie="$REPO/target/debug/dassie"
want_code=3
# The jailed boot's uid and gid, as scripts/host/jail-env.sh gives index 0.
jail_id=$((0x70000000))
# On $OUT's filesystem, so the assets can be hard-linked in.
jail_base="$OUT/smoke/jail"

rootfs="$OUT/rootfs.erofs"
before=$(sha256sum "$rootfs")

# boot N direct|systemd: boots the VM once, with its files in $OUT/smoke/N.
boot() {
  local n=$1 mode=$2
  local run="$OUT/smoke/$n"
  # A systemd boot leaves root-owned files; the directory is ours to empty.
  rm -rf "$run"
  mkdir -p "$run"
  "$REPO/scripts/host/vm-config.sh" "$kernel" "$OUT/initramfs.cpio.gz" "$rootfs" "$run/vm.vsock" \
    "dassie.instance=smoke-$n dassie.env=EXIT_AFTER=3 dassie.env=EXIT_CODE=$want_code" \
    >"$run/config.json"
  local vm_run=("$dassie" vm-run --config "$run/config.json" --firecracker "$OUT/firecracker")

  log "booting $ARCH VM ($n, $mode)"
  local status=0
  case "$mode" in
    direct)
      timeout --kill-after=10 "$timeout_secs" "${vm_run[@]}" \
        >"$run/console.log" 2>"$run/vm-run.log" || status=$?
      ;;
    systemd)
      # The jail belongs to the jail uid, and only root can remove it.
      sudo -n rm -rf "$jail_base"
      # systemd runs ExecStartPost only once READY=1 has arrived, so the
      # marker proves vm-run notified. An exit before READY=1 also ends in
      # `exit-code`. RuntimeMaxSec only counts once the unit is active, so
      # TimeoutStartSec bounds a boot that never becomes ready. systemd splits
      # the ExecStartPost command line itself, hence the inner quotes.
      sudo -n systemd-run --wait --collect --unit "dassie-smoke-$n-$$" \
        -p Type=notify -p NotifyAccess=main -p KillMode=mixed \
        -p TimeoutStartSec="$timeout_secs" -p RuntimeMaxSec="$timeout_secs" \
        -p TimeoutStopSec=30 \
        -p "ExecStartPost=$(command -v touch) \"$run/notified\"" \
        -p StandardOutput="file:$run/console.log" -p StandardError="file:$run/vm-run.log" \
        "${vm_run[@]}" --id "smoke-$n" --uid "$jail_id" --gid "$jail_id" \
        --jailer "$OUT/jailer" --chroot-base "$jail_base" \
        2>"$run/systemd-run.log" || true
      if ! grep -qF "Finished with result: exit-code" "$run/systemd-run.log" ||
        [[ ! -f "$run/notified" ]]; then
        status=-1
      else
        status=$(sed -n 's/.*code=exited.*status=\([0-9]*\).*/\1/p' "$run/systemd-run.log")
      fi
      ;;
  esac

  dump() {
    echo "--- console ---" && cat "$run/console.log"
    echo "--- vm-run ---" && cat "$run/vm-run.log"
    if [[ -f "$run/systemd-run.log" ]]; then
      echo "--- systemd-run ---" && cat "$run/systemd-run.log"
    fi
  }

  if [[ "$status" != "$want_code" ]]; then
    dump
    echo "FAIL: boot $n ($mode): vm-run exited with ${status:-?}, want $want_code" \
      "(124: timed out, 196: no exited event, 197: VM failed, -1: unit did not end in exit-code, or never got READY=1)" >&2
    exit 1
  fi
  if ! awk '/event=started/ { s = NR } /event=ready/ && s { r = NR }
      /event=exited code='"$want_code"'/ && r { ok = 1 } END { exit !ok }' "$run/vm-run.log"; then
    dump
    echo "FAIL: boot $n ($mode): events not in started, ready, exited order" >&2
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
      echo "FAIL: boot $n ($mode): console has no \"$want\"" >&2
      exit 1
    fi
  done
  if [[ "$mode" == systemd ]]; then
    local owner
    owner=$(sudo -n stat -c %u:%g "$jail_base/firecracker/smoke-$n/root")
    if [[ "$owner" != "$jail_id:$jail_id" ]]; then
      dump
      echo "FAIL: boot $n ($mode): jail root is owned by $owner, want $jail_id:$jail_id" >&2
      exit 1
    fi
    echo "jail: $jail_base/firecracker/smoke-$n/root owned by $owner"
  fi
  grep -F "vm-run: event=" "$run/vm-run.log"
}

if command -v systemd-run >/dev/null && sudo -n true 2>/dev/null; then
  second=systemd
else
  log "no systemd-run or passwordless sudo: booting twice without systemd or the jailer"
  second=direct
fi

boot 1 direct
boot 2 "$second"

if [[ "$(sha256sum "$rootfs")" != "$before" ]]; then
  echo "FAIL: the read-only rootfs changed: $before" >&2
  exit 1
fi
[[ "$second" == systemd ]] && second="systemd + jailer"
log "PASS: two boots of one read-only rootfs (direct, $second): started, ready, exited;" \
  "vm-run exited $want_code; non-root /dev ok; clean overlay root each time; rootfs unchanged"
