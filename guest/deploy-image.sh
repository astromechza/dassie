#!/usr/bin/env bash
# One command from an OCI image reference to a VM served by Caddy (issue #10).
#
#   guest/deploy-image.sh REF NAME INDEX [HOSTNAME [PORT]]
#
#   guest/deploy-image.sh docker.io/traefik/whoami:latest web 10 web.localhost
#
# M1 scaffolding: the daemon replaces this script in M2. What lasts is the
# Caddy config it writes (ADR 0010) and the rule that redeploying the same
# image digest changes nothing; the per-VM files, the hand-picked INDEX and
# the bash rendering go.
#
# Pulls REF and builds its rootfs (dassie-image, ADR 0007), then writes the
# VM's files under /var/lib/dassie/vms/NAME:
#
#   net.env      guest/net-env.sh INDEX (TAP, /31, MAC, DNS; ADR 0008)
#   config.json  guest/vm-config.sh, with the rootfs and net.env
#   route.env    DASSIE_HOSTNAME and DASSIE_PORT, only with a HOSTNAME
#
# and renders /etc/dassie/caddy.json from every VM's route.env. A file is
# only replaced when its content changes, and fc@NAME is only restarted when
# its files did: redeploying a reference that resolves to the same digest
# pulls no blobs, builds nothing and restarts nothing. Caddy is reloaded only
# when caddy.json changed.
#
# INDEX picks the VM's /31 and TAP, and must not be another VM's. HOSTNAME
# routes https://HOSTNAME/ to the guest's PORT (default 80); without it the
# VM gets no route. Run it as a user with sudo, from a checkout: cargo builds
# as you, everything else runs through sudo. Run one deploy at a time.
#
# Needs the host set up as in guest/systemd/README.md: guest assets,
# firecracker, dassie-listener, caddy and the units. Plus a Rust toolchain
# and erofs-utils for dassie-image.

set -euo pipefail
shopt -s inherit_errexit
# Glob order (the routes in caddy.json) must not depend on the locale.
export LC_ALL=C

here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")
vms=/var/lib/dassie/vms
guest=/var/lib/dassie/guest
caddy_json=/etc/dassie/caddy.json

die() {
  echo "deploy-image.sh: $*" >&2
  exit 2
}
log() { echo "==> $*" >&2; }

if (($# < 3 || $# > 5)); then
  echo "usage: $0 REF NAME INDEX [HOSTNAME [PORT]]" >&2
  exit 2
fi
ref=$1 name=$2 index=$3 hostname=${4:-} port=${5:-80}

ip4='([0-9]{1,3}\.){3}[0-9]{1,3}'
label='[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?'
host_re="^($label\.)*$label\$"
port_re='^[1-9][0-9]{0,4}$'

[[ "$name" =~ ^[a-z0-9][a-z0-9-]{0,31}$ ]] || die "NAME must be lowercase letters, digits and -, got ${name@Q}"
if [[ -n "$hostname" ]]; then
  [[ "$hostname" =~ $host_re ]] || die "HOSTNAME must be a lowercase DNS name, got ${hostname@Q}"
fi
if [[ ! "$port" =~ $port_re ]] || ((port > 65535)); then
  die "PORT must be 1..65535, got ${port@Q}"
fi

sudo=()
((EUID == 0)) || sudo=(sudo)

case "$(uname -m)" in
  x86_64) kernel=$guest/vmlinux ;;
  aarch64) kernel=$guest/Image ;;
  *) die "unsupported host arch $(uname -m)" ;;
esac
for need in "$kernel" "$guest/initramfs.cpio.gz" /usr/local/bin/firecracker \
  /usr/local/bin/dassie-listener /usr/local/bin/caddy /etc/dassie/dassie.nft; do
  [[ -e "$need" ]] || die "$need is missing; set up the host as in guest/systemd/README.md"
done

# get FILE KEY REGEX -> prints KEY's value from an env file, if it matches
# REGEX. The file is read, not sourced, like vm-config.sh does.
get() {
  local file=$1 want=$2 re=$3 key value found=""
  while IFS='=' read -r key value || [[ -n "$key" ]]; do
    [[ "$key" == "$want" ]] && found=$value
  done <"$file"
  [[ "$found" =~ $re ]] || die "$file: bad or missing $want=$found"
  printf '%s' "$found"
}

# Another VM on this INDEX would share its TAP name, and starting this one
# would delete that VM's live TAP.
for env in "$vms"/*/net.env; do
  [[ -e "$env" && "$env" != "$vms/$name/net.env" ]] || continue
  other=$(get "$env" DASSIE_INDEX '^[0-9]+$')
  if [[ "$other" == "$index" ]]; then
    die "INDEX $index is already used by $(basename "$(dirname "$env")")"
  fi
done

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# --- Image: pull fetches only missing blobs, build skips an existing file.
log "building the image tool"
cargo build --quiet --locked --release -p dassie-image --example image --manifest-path "$repo/Cargo.toml"
image_tool=${CARGO_TARGET_DIR:-$repo/target}/release/examples/image
log "pulling $ref"
digest=$("${sudo[@]}" "$image_tool" pull "$ref")
log "building the rootfs for $digest"
rootfs=$("${sudo[@]}" "$image_tool" build "$digest")

# --- The VM's files, staged, then compared with the live ones.
"$here/net-env.sh" "$index" >"$stage/net.env"
FC_NET_ENV=$stage/net.env "$here/vm-config.sh" "$kernel" "$guest/initramfs.cpio.gz" \
  "$rootfs" "/run/dassie/$name.vsock" "dassie.instance=$name" >"$stage/config.json"
if [[ -n "$hostname" ]]; then
  printf 'DASSIE_HOSTNAME=%s\nDASSIE_PORT=%s\n' "$hostname" "$port" >"$stage/route.env"
fi

# same SRC DEST -> succeeds if DEST exists with SRC's content.
same() { "${sudo[@]}" cmp -s "$1" "$2"; }
# put SRC DEST -> replaces DEST with SRC by a rename, so a crash never
# leaves half a file. The syncs make it survive a power cut too: without
# them, a file renamed into place less than ~30 s before the cut comes back
# empty on ext4, and the VM or Caddy fails to start at boot.
put() {
  "${sudo[@]}" install -m 0644 "$1" "$2.tmp"
  "${sudo[@]}" sync "$2.tmp"
  "${sudo[@]}" mv -f "$2.tmp" "$2"
  "${sudo[@]}" sync "$(dirname "$2")"
}

dir=$vms/$name
vm_changed=0
for f in net.env config.json; do
  same "$stage/$f" "$dir/$f" || vm_changed=1
done

unit=fc@$name.service
if ((vm_changed)); then
  # Stop before replacing net.env: the TAP unit's ExecStop reads it to find
  # the TAP to delete. The TAP goes with the VM (StopWhenUnneeded).
  "${sudo[@]}" systemctl stop "$unit"
  "${sudo[@]}" install -d -m 0755 "$dir"
  "${sudo[@]}" sync "$vms"
  for f in net.env config.json; do put "$stage/$f" "$dir/$f"; done
fi
if [[ -n "$hostname" ]]; then
  same "$stage/route.env" "$dir/route.env" || put "$stage/route.env" "$dir/route.env"
else
  "${sudo[@]}" rm -f "$dir/route.env"
fi
# Enabled, so it comes back after a host reboot. A no-op on a running VM.
"${sudo[@]}" systemctl enable --quiet --now dassie-net.service "$unit"

# --- Caddy: one route per VM with a route.env (ADR 0010).
# json_route NAME HOSTNAME DIAL -> one element of the server's routes.
json_route() {
  cat <<EOF
            {
              "@id": "vm-$1",
              "match": [{ "host": ["$2"] }],
              "handle": [{ "handler": "reverse_proxy", "upstreams": [{ "dial": "$3" }] }],
              "terminal": true
            }
EOF
}

render_caddy() {
  local env vm host vm_port guest_ip sep="" routes=""
  local -A seen=()
  for env in "$vms"/*/route.env; do
    [[ -e "$env" ]] || continue
    vm=$(basename "$(dirname "$env")")
    host=$(get "$env" DASSIE_HOSTNAME "$host_re")
    vm_port=$(get "$env" DASSIE_PORT "$port_re")
    guest_ip=$(get "$(dirname "$env")/net.env" DASSIE_GUEST_IP "^$ip4\$")
    [[ -z "${seen[$host]:-}" ]] || die "$host is routed to both ${seen[$host]} and $vm"
    seen[$host]=$vm
    routes+="$sep$(json_route "$vm" "$host" "$guest_ip:$vm_port")"
    sep=$',\n'
  done
  # tls_connection_policies: :443 is TLS-only even with no routes.
  # The `internal` issuer signs every name from Caddy's own local CA, and
  # install_trust: false keeps that CA out of the host's trust store.
  cat <<EOF
{
  "admin": {
    "listen": "unix//run/caddy/admin.sock",
    "config": { "persist": false }
  },
  "storage": { "module": "file_system", "root": "/var/lib/caddy" },
  "apps": {
    "http": {
      "servers": {
        "dassie": {
          "listen": [":443"],
          "tls_connection_policies": [{}],
          "routes": [
$routes
          ]
        }
      }
    },
    "tls": {
      "automation": { "policies": [{ "issuers": [{ "module": "internal" }] }] }
    },
    "pki": {
      "certificate_authorities": { "local": { "install_trust": false } }
    }
  }
}
EOF
}

render_caddy >"$stage/caddy.json"
"${sudo[@]}" systemctl enable --quiet caddy.service
if same "$stage/caddy.json" "$caddy_json"; then
  caddy_changed=0
  "${sudo[@]}" systemctl start caddy.service
else
  caddy_changed=1
  put "$stage/caddy.json" "$caddy_json"
  "${sudo[@]}" systemctl reload-or-restart caddy.service
fi

guest_ip=$(get "$dir/net.env" DASSIE_GUEST_IP "^$ip4\$")
echo "image:  $ref"
echo "digest: $digest"
echo "rootfs: $rootfs"
echo "vm:     $unit at $guest_ip, $( ((vm_changed)) && echo "(re)started" || echo "unchanged, not restarted")"
if [[ -n "$hostname" ]]; then
  echo "route:  https://$hostname/ -> $guest_ip:$port"
else
  echo "route:  none"
fi
echo "caddy:  $( ((caddy_changed)) && echo "reloaded" || echo "unchanged, not reloaded")"
