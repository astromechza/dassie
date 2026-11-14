#!/usr/bin/env bash
# Prints the `jail.env` for one VM index (ADR 0013): the uid and gid its
# Firecracker runs as in the jailer, and that owns its TAP.
#
#   scripts/host/jail-env.sh INDEX >/var/lib/dassie/vms/NAME/jail.env
#
# INDEX is 0..32767, the same index as the VM's net.env, and must be unique
# across all VMs, networked or not. The uid and gid are 0x70000000 + INDEX
# (1879048192 and up): above systemd's container range, where rootless
# containers' subuids usually sit, and below the upper half of the uid
# space. They need no passwd entry. Read by `fc@.service` and
# `dassie-tap@.service` (systemd `EnvironmentFile=`). M2's daemon allocates
# them instead.

set -euo pipefail

if (($# != 1)) || [[ ! "$1" =~ ^(0|[1-9][0-9]{0,4})$ ]] || (($1 > 32767)); then
  echo "usage: $0 INDEX (0..32767)" >&2
  exit 2
fi
id=$((0x70000000 + $1))
printf 'DASSIE_UID=%d\nDASSIE_GID=%d\n' "$id" "$id"
