#!/usr/bin/env bash
# Prints a Firecracker `--config-file` for one VM: shared kernel and
# initramfs, the app rootfs as a read-only drive, and a vsock device.
#
# The drive is always read-only, even when the file is 0444: Firecracker run
# as root would open it read-write, and a root process in the guest could
# then write /dev/vda. dassie-init overlays a tmpfs on it, so every VM of an
# image shares the file and every boot starts clean (ADR 0006).
#
#   guest/vm-config.sh KERNEL INITRD ROOTFS VSOCK_UDS [EXTRA_BOOT_ARGS]
#
# EXTRA_BOOT_ARGS carries per-VM `dassie.*` args (ADR 0005), and `ip=` when a
# NIC is added, and may use the kernel's double quotes
# (`dassie.env="GREETING=hello world"`).
#
# Set FC_TAP to a host TAP device name to attach it as the guest's eth0. Its
# address comes from an `ip=GUEST::GATEWAY:NETMASK::eth0:off` boot arg.

set -euo pipefail

if (($# < 4)); then
  echo "usage: $0 KERNEL INITRD ROOTFS VSOCK_UDS [EXTRA_BOOT_ARGS]" >&2
  exit 2
fi
kernel=$1 initrd=$2 rootfs=$3 uds=$4 extra=${5:-}

# reboot=k: on x86_64, reboot through the i8042 reset line, which Firecracker
# turns into a clean exit. panic=1: a guest kernel panic also reboots.
boot_args="console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro${extra:+ $extra}"

# Control characters have no place in a path or the cmdline, so they are
# refused here rather than escaped. This runs in the main shell: an exit
# inside the heredoc's $(...) below would not stop the script.
for value in "$kernel" "$initrd" "$rootfs" "$uds" "$boot_args" "${FC_TAP:-}"; do
  if [[ "$value" =~ [[:cntrl:]] ]]; then
    echo "vm-config.sh: control character in ${value@Q}" >&2
    exit 2
  fi
done

# json STRING -> STRING as a JSON string literal.
json() {
  local s=${1//\\/\\\\}
  printf '"%s"' "${s//\"/\\\"}"
}

network=""
if [[ -n "${FC_TAP:-}" ]]; then
  network=$(printf ',\n  "network-interfaces": [\n    { "iface_id": "eth0", "host_dev_name": %s }\n  ]' "$(json "$FC_TAP")")
fi

cat <<EOF
{
  "boot-source": {
    "kernel_image_path": $(json "$kernel"),
    "initrd_path": $(json "$initrd"),
    "boot_args": $(json "$boot_args")
  },
  "drives": [
    {
      "drive_id": "rootfs",
      "path_on_host": $(json "$rootfs"),
      "is_root_device": false,
      "is_read_only": true
    }
  ],
  "machine-config": {
    "vcpu_count": 1,
    "mem_size_mib": 256
  },
  "vsock": {
    "guest_cid": 3,
    "uds_path": $(json "$uds")
  }$network
}
EOF
