#!/usr/bin/env bash
# Tests the VM network policy (guest/systemd/dassie.nft, ADR 0008) without
# KVM: network namespaces stand in for the host, two VMs and the internet.
#
#   sudo guest/net-policy-test.sh
#
#   dpt-vm1 --(dt00001)--+
#                        +-- dpt-host --(up0)-- dpt-world
#   dpt-vm2 --(dt00002)--+   (dassie.nft)       (answers on every IPv4
#                                                address, like the internet)
#
# The VMs' addresses come from guest/net-env.sh, and the host end of each
# veth is set up as dassie-tap@.service sets up a TAP. A fixed matrix checks
# each flow in the ADR. Then NET_POLICY_CASES random TCP probes (default
# 200, seed NET_POLICY_SEED) are checked against a model of the policy:
# a VM address, a host address or @no_egress is refused, and anything else
# reaches the world. Needs root, iproute2, nftables and python3. The
# host's own network and firewall are not touched. NET_POLICY_NFT tests
# another ruleset file instead, e.g. to check that a broken one fails.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
cases=${NET_POLICY_CASES:-200}
nft_file=${NET_POLICY_NFT:-$here/systemd/dassie.nft}
seed=${NET_POLICY_SEED:-$(date +%s)}
RANDOM=$seed
uplink=198.18.0.1 world_ip=198.18.0.2
failed=0 checked=0 pids=()

if ((EUID != 0)); then
  echo "$0: run as root" >&2
  exit 2
fi

cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for ns in dpt-vm1 dpt-vm2 dpt-host dpt-world; do ip netns del "$ns" 2>/dev/null || true; done
}
trap cleanup EXIT
cleanup

nsx() { ip netns exec "$@"; }

# vm NAME INDEX: a fake VM behind dt<INDEX>, addressed from net-env.sh.
declare -A guest_ip host_ip
vm() {
  local name=$1 index=$2 key value
  local -A env=()
  while IFS='=' read -r key value; do env[$key]=$value; done < <("$here/net-env.sh" "$index")
  ip netns add "$name"
  ip -n dpt-host link add "${env[DASSIE_TAP]}" type veth peer name eth0 netns "$name"
  nsx dpt-host sysctl -q -w "net.ipv6.conf.${env[DASSIE_TAP]}.disable_ipv6=1"
  ip -n dpt-host addr add "${env[DASSIE_HOST_IP]}/31" dev "${env[DASSIE_TAP]}"
  ip -n dpt-host link set dev "${env[DASSIE_TAP]}" multicast off up
  ip -n "$name" link set dev eth0 address "${env[DASSIE_MAC]}" up
  ip -n "$name" addr add "${env[DASSIE_GUEST_IP]}/31" dev eth0
  ip -n "$name" route add default via "${env[DASSIE_HOST_IP]}"
  guest_ip[$name]=${env[DASSIE_GUEST_IP]} host_ip[$name]=${env[DASSIE_HOST_IP]}
}

# A TCP listener on every address, for the probes to connect to. It logs
# each connection's source (`ADDR accepted`) to stderr, which the masquerade
# check reads, and prints `listening` on stdout once it is.
listen_py='
import socket, sys
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", int(sys.argv[1])))
s.listen(1024)
print("listening", flush=True)
while True:
    conn, (addr, _) = s.accept()
    print(addr, "accepted", file=sys.stderr, flush=True)
    conn.close()
'

# serve NS PORT LOG: the listener in NS, its stderr in LOG. Returns once it
# listens, so the first probe cannot race its startup. A listener that dies
# fails the test at once, with its stderr. One that is slow to start (a busy
# CI runner) gets 30 s.
serve() {
  local fd line
  exec {fd}< <(exec ip netns exec "$1" python3 -I -c "$listen_py" "$2" 2>"$3")
  pids+=($!)
  if read -r -t 30 -u "$fd" line && [[ "$line" == listening ]]; then
    exec {fd}<&-
    return
  fi
  echo "$0: no listener on port $2 in $1 after 30 s, or it exited. Its stderr:" >&2
  cat "$3" >&2
  exit 1
}

# --- Topology --------------------------------------------------------------

ip netns add dpt-host
ip netns add dpt-world
ip -n dpt-host link set lo up
ip -n dpt-world link set lo up
ip -n dpt-host link add up0 type veth peer name up0 netns dpt-world
ip -n dpt-host addr add "$uplink/30" dev up0
ip -n dpt-world addr add "$world_ip/30" dev up0
ip -n dpt-host link set up0 up
ip -n dpt-world link set up0 up
ip -n dpt-host route add default via "$world_ip"
# AnyIP: the world owns every IPv4 address, so any egress gets an answer.
ip -n dpt-world route add local 0.0.0.0/0 dev lo
# Spoofed probes use port 7 only. A reply to a forged source goes to the
# address's owner, not the prober, so the prober alone cannot tell "dropped"
# from "sent". This counter shows whether any of them left the host.
nsx dpt-world nft 'table ip world { chain input { type filter hook input priority filter; tcp dport 7 counter; }; }'
nsx dpt-host sysctl -q -w net.ipv4.ip_forward=1
# The kernel sends at most about one ICMP error per second to each peer
# after a short burst, and every refusal here goes to the same fake VM. Lift
# the limit in this namespace only, so each probe gets its answer at once.
nsx dpt-host sysctl -q -w net.ipv4.icmp_ratelimit=0
nsx dpt-host nft -f "$nft_file"
vm dpt-vm1 1
vm dpt-vm2 2

logs=$(mktemp -d)
serve dpt-world 80 "$logs/world.log"
serve dpt-host 8080 "$logs/host.log"
serve dpt-vm1 8080 "$logs/vm1.log"
serve dpt-vm2 8080 "$logs/vm2.log"

# --- Probes ----------------------------------------------------------------

# One TCP connect, run inside a namespace. A non-blocking connect with a
# timeout, as apps do: bash's /dev/tcp blocks, and a blocking connect only
# reports ICMP admin-prohibited after its SYN is retransmitted, a second late.
#   open:    connected
#   reached: the destination itself reset it (nothing listening there)
#   refused: ICMP admin-prohibited from dassie.nft (EHOSTUNREACH)
#   dropped: no answer
probe_py='
import errno, socket, sys
dst, port, src = sys.argv[1], int(sys.argv[2]), sys.argv[3]
s = socket.socket()
s.settimeout(2)
if src:
    s.bind((src, 0))
try:
    s.connect((dst, port))
    print("open")
except socket.timeout:
    print("dropped")
except OSError as e:
    print({errno.ECONNREFUSED: "reached", errno.EHOSTUNREACH: "refused"}.get(e.errno, f"error: {e}"))
'

# tcp NS DST PORT -> the probe from the VM's own address.
tcp() {
  nsx "$1" python3 -c "$probe_py" "$2" "$3" ""
}

# spoof NS SRC DST PORT -> the probe from an address that is not the VM's.
spoof() {
  ip -n "$1" addr add "$2/32" dev eth0
  nsx "$1" python3 -c "$probe_py" "$3" "$4" "$2"
  ip -n "$1" addr del "$2/32" dev eth0
}

# ping NS DST -> ok | refused | dropped
ping4() {
  local out
  out=$(nsx "$1" ping -c1 -W1 "$2" 2>&1) && { echo ok; return; }
  [[ "$out" == *"1 received"* ]] && echo ok && return
  [[ "$out" == *errors* || "$out" == *rohibited* ]] && echo refused || echo dropped
}

expect() {
  local want=$1 got=$2 what=$3
  checked=$((checked + 1))
  if [[ "$got" == "$want" || ("$want" == reach && "$got" =~ ^(open|reached)$) ]]; then
    printf 'ok    %-8s %s\n' "$got" "$what"
  else
    printf 'FAIL  want %s, got %s: %s\n' "$want" "$got" "$what"
    failed=1
  fi
}

vm1=${guest_ip[dpt-vm1]} vm2=${guest_ip[dpt-vm2]}
gw1=${host_ip[dpt-vm1]} gw2=${host_ip[dpt-vm2]}

echo "## fixed matrix"
expect open "$(tcp dpt-host "$vm1" 8080)" "host -> vm1"
expect open "$(tcp dpt-host "$vm2" 8080)" "host -> vm2"
expect refused "$(tcp dpt-vm1 "$vm2" 8080)" "vm1 -> vm2"
expect refused "$(tcp dpt-vm2 "$vm1" 8080)" "vm2 -> vm1"
expect refused "$(tcp dpt-vm1 "$gw1" 8080)" "vm1 -> its gateway"
expect refused "$(tcp dpt-vm1 "$gw2" 8080)" "vm1 -> vm2's gateway"
expect refused "$(tcp dpt-vm1 "$uplink" 8080)" "vm1 -> host uplink address"
for dst in 169.254.169.254 10.1.2.3 172.17.0.2 192.168.1.1 100.64.0.1; do
  expect refused "$(tcp dpt-vm1 "$dst" 80)" "vm1 -> $dst (no_egress)"
done
expect open "$(tcp dpt-vm1 1.1.1.1 80)" "vm1 -> 1.1.1.1 (egress)"
expect open "$(tcp dpt-vm2 "$world_ip" 80)" "vm2 -> world uplink"
# The world's log holds the source of every connection it accepted. VM
# egress must arrive from the uplink address, never from a VM's own.
if grep -q "^$uplink " "$logs/world.log" && ! grep -q '^10\.88\.' "$logs/world.log"; then
  expect masq masq "egress arrives masqueraded as $uplink"
else
  expect masq "$(cut -d' ' -f1 "$logs/world.log" | sort -u | tr '\n' ' ')" "egress masqueraded"
fi
expect dropped "$(spoof dpt-vm1 "$vm2" 1.1.1.1 7)" "vm1 spoofing vm2's address"
expect dropped "$(spoof dpt-vm1 203.0.113.7 1.1.1.1 7)" "vm1 spoofing an outside address"
expect 0 "$(nsx dpt-world nft list chain ip world input | grep -o 'packets [0-9]*' | cut -d' ' -f2)" \
  "spoofed packets that reached the world"
expect refused "$(ping4 dpt-vm1 "$vm2")" "ping vm1 -> vm2"
expect refused "$(ping4 dpt-vm1 "$gw1")" "ping vm1 -> its gateway"
expect ok "$(ping4 dpt-vm1 1.1.1.1)" "ping vm1 -> 1.1.1.1"
ip -n dpt-world route add 10.88.0.0/16 via "$uplink"
expect dropped "$(tcp dpt-world "$vm1" 8080)" "world -> vm1 (unsolicited)"
expect none "$(ip -n dpt-host -6 -o addr show dev dt00001 | wc -l | sed 's/^ *0$/none/')" "no IPv6 on the host end"

# --- Random probes against a model ------------------------------------------

ip2int() {
  local IFS=. a b c d
  read -r a b c d <<<"$1"
  echo $(((a << 24) | (b << 16) | (c << 8) | d))
}
# in_cidr IP CIDR
in_cidr() {
  local ip net bits
  ip=$(ip2int "$1") net=$(ip2int "${2%/*}") bits=${2#*/}
  (((ip ^ net) >> (32 - bits) == 0))
}
no_egress=(10.0.0.0/8 100.64.0.0/10 169.254.0.0/16 172.16.0.0/12 192.168.0.0/16)
# model DST -> what the policy says a TCP connect from a VM should see.
model() {
  local cidr
  in_cidr "$1" 10.88.0.0/16 && { echo refused; return; }
  [[ "$1" == "$uplink" ]] && { echo refused; return; }
  for cidr in "${no_egress[@]}"; do
    in_cidr "$1" "$cidr" && { echo refused; return; }
  done
  echo reach
}
rand8() { echo $((RANDOM % 256)); }
# A destination from one of the classes the model distinguishes. Skips
# addresses no VM could meaningfully dial (0/8, loopback, multicast and up,
# the uplink /30's network and broadcast) and vm1's own address.
random_dst() {
  local dst first
  while true; do
    case $((RANDOM % 4)) in
      0) dst="10.88.$(rand8).$(rand8)" ;;
      1)
        local cidr=${no_egress[RANDOM % ${#no_egress[@]}]} base span
        base=$(ip2int "${cidr%/*}") span=$((1 << (32 - ${cidr#*/})))
        local n=$((base + (RANDOM * 32768 + RANDOM) % span))
        dst="$((n >> 24 & 255)).$((n >> 16 & 255)).$((n >> 8 & 255)).$((n & 255))"
        ;;
      2) dst=$uplink ;;
      *) first=$((1 + RANDOM % 223)) dst="$first.$(rand8).$(rand8).$(rand8)" ;;
    esac
    first=${dst%%.*}
    ((first == 0 || first == 127 || first >= 224)) && continue
    [[ "$dst" == "$vm1" || "$dst" == 198.18.0.0 || "$dst" == 198.18.0.3 ]] && continue
    echo "$dst"
    return
  done
}

echo "## $cases random probes from vm1 (NET_POLICY_SEED=$seed)"
for ((i = 0; i < cases; i++)); do
  dst=$(random_dst) port=$((1 + RANDOM % 65535))
  want=$(model "$dst") got=$(tcp dpt-vm1 "$dst" "$port")
  if [[ "$got" == "$want" || ("$want" == reach && "$got" =~ ^(open|reached)$) ]]; then
    checked=$((checked + 1))
  else
    expect "$want" "$got" "vm1 -> $dst:$port"
  fi
done

echo "## counters"
nsx dpt-host nft list table inet dassie | grep counter
if ((failed)); then
  echo "FAIL: policy test (NET_POLICY_SEED=$seed)" >&2
  exit 1
fi
echo "PASS: $checked checks"
