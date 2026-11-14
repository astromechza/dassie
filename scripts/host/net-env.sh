#!/usr/bin/env bash
# Prints the `net.env` for one VM index (ADR 0008): its TAP name, the /31's
# host and guest addresses, the guest MAC and its DNS resolvers. The TAP is
# dt plus the index padded to 5 digits (dt00042), so names sort by index.
#
#   scripts/host/net-env.sh INDEX >/var/lib/dassie/vms/NAME/net.env
#
# INDEX is 0..32767 and must be unique per running VM. VM N gets the /31 at
# POOL.0.0 + 2N: the host side is the even address and the guest's gateway,
# the guest the odd one. The output is read by `dassie-tap@.service`
# (systemd `EnvironmentFile=`) and by `scripts/host/vm-config.sh` (FC_NET_ENV).
#
# DASSIE_POOL sets the VM /16 as its first two octets (default 10.88, i.e.
# 10.88.0.0/16). It must match the `vm_pool` set in systemd/dassie.nft.
# DASSIE_DNS overrides the resolvers (comma-separated, at most 3).

set -euo pipefail

if (($# != 1)) || [[ ! "$1" =~ ^(0|[1-9][0-9]{0,4})$ ]] || (($1 > 32767)); then
  echo "usage: $0 INDEX (0..32767)" >&2
  exit 2
fi
index=$1
pool=${DASSIE_POOL:-10.88}
if [[ ! "$pool" =~ ^(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})$ ]] ||
  ((BASH_REMATCH[1] > 255 || BASH_REMATCH[2] > 255)); then
  echo "$0: DASSIE_POOL must be the first two octets of a /16, e.g. 10.88, got ${pool@Q}" >&2
  exit 2
fi
pool_a=${BASH_REMATCH[1]} pool_b=${BASH_REMATCH[2]}
dns=${DASSIE_DNS:-1.1.1.1,9.9.9.9}
# A loose shape check; dassie-init parses each address and refuses bad ones.
if [[ ! "$dns" =~ ^[0-9a-fA-F.:]+(,[0-9a-fA-F.:]+){0,2}$ ]]; then
  echo "$0: DASSIE_DNS must be 1 to 3 comma-separated IPs, got ${dns@Q}" >&2
  exit 2
fi

host=$((index * 2))
guest=$((host + 1))
# POOL.X.Y: X and Y are the high and low bytes of the offset into the /16.
host_ip="$pool.$((host >> 8)).$((host & 255))"
guest_ip="$pool.$((guest >> 8)).$((guest & 255))"
# Locally administered unicast (06:00:...) followed by the guest IP, as in
# Firecracker's examples, so a MAC in a capture names its VM.
mac=$(printf '06:00:%02x:%02x:%02x:%02x' "$pool_a" "$pool_b" $((guest >> 8)) $((guest & 255)))

tap=$(printf 'dt%05d' "$index")

cat <<EOF
DASSIE_INDEX=$index
DASSIE_TAP=$tap
DASSIE_HOST_IP=$host_ip
DASSIE_GUEST_IP=$guest_ip
DASSIE_MAC=$mac
DASSIE_DNS=$dns
EOF
