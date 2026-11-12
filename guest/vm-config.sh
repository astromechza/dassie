#!/usr/bin/env bash
# Prints a Firecracker `--config-file` for one VM: shared kernel and
# initramfs, the app rootfs as a read-write drive, and a vsock device.
#
#   guest/vm-config.sh KERNEL INITRD ROOTFS VSOCK_UDS [EXTRA_BOOT_ARGS]
#
# EXTRA_BOOT_ARGS carries per-VM `dassie.*` args (ADR 0005), and `ip=` when a
# NIC is added. Values must not need JSON escaping.
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
boot_args="console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw${extra:+ $extra}"

network=""
if [[ -n "${FC_TAP:-}" ]]; then
  network=$(printf ',\n  "network-interfaces": [\n    { "iface_id": "eth0", "host_dev_name": "%s" }\n  ]' "$FC_TAP")
fi

cat <<EOF
{
  "boot-source": {
    "kernel_image_path": "$kernel",
    "initrd_path": "$initrd",
    "boot_args": "$boot_args"
  },
  "drives": [
    {
      "drive_id": "rootfs",
      "path_on_host": "$rootfs",
      "is_root_device": false,
      "is_read_only": false
    }
  ],
  "machine-config": {
    "vcpu_count": 1,
    "mem_size_mib": 256
  },
  "vsock": {
    "guest_cid": 3,
    "uds_path": "$uds"
  }$network
}
EOF
