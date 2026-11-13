# 0008. Guest networking

Status: Accepted
Date: 2026-11-13

## Context

M1 gives VMs a network, for Caddy's ingress and the app's egress. Several
forces shape it:

- **Isolation by default.** Apps on one host belong to different owners. A VM
  must not reach another VM, or a service on the host, unless a later policy
  allows it.
- **No daemon at runtime.** As with `fc@.service` (ADR 0006), the network must
  come back after a host reboot from systemd units and files alone. M2's daemon
  will generate those files, not sit in the data path.
- **Scale.** One host should be able to run thousands of small VMs. Per-VM
  rules or per-VM bridge ports would grow with them.
- **Later milestones.** M4 swaps blue and green behind a stable address. M5
  clones VMs from snapshots, and the clones share one guest address.
- **Networking is optional.** Readiness, health and stop travel over vsock
  (ADR 0004, ADR 0005). A VM with no ingress, or no NIC at all, still has a
  full lifecycle.

## Decision

**A routed TAP per VM, with no bridge.**

- VM index `N` (0..32767) gets the /31 at `10.88.0.0 + 2N` from
  `10.88.0.0/16`. The host end is the even address and the guest's gateway. The
  guest end is the odd one.
- The TAP is named `dt<N>`, at most 7 characters, well inside IFNAMSIZ (15).
- The guest MAC is `06:00:` followed by the guest IP in hex: locally
  administered and unicast, as in Firecracker's examples, so a capture names
  its VM.
- The guest gets its address from the kernel's `ip=` autoconfig
  (`CONFIG_IP_PNP`, pinned in `guest/kernel/dassie.config`), as
  `ip=GUEST::HOST:255.255.255.254::eth0:off`. No DHCP runs anywhere.
- The TAP is IPv4 only, with IPv6 and multicast off on the host end. That
  way the host's own link-local services (mDNS, LLMNR, router solicitations)
  never announce it to a guest.

**Per-VM state is one file, `/var/lib/dassie/vms/<name>/net.env`.** It holds
the index, TAP name, host IP, guest IP, MAC and DNS resolvers, in systemd
`EnvironmentFile=` syntax. `guest/net-env.sh <index>` writes it in M1. M2
allocates it in SQLite, per VM instance rather than per app, so blue and green
get different addresses.

**Host units, using `ip` and `nft` rather than systemd-networkd.** That way
dassie does not depend on, or fight, whichever network manager the host uses.

- `dassie-net.service` (oneshot) enables `net.ipv4.ip_forward`, sizes the
  neighbour table, and loads the `inet dassie` nft table. `reload` swaps the
  table atomically without restarting any VM.
- `dassie-tap@<name>.service` (oneshot, `RemainAfterExit`) creates `dt<N>`
  from `net.env`, gives it the host address, and deletes it on stop.
- A drop-in makes every `fc@<name>` require its TAP unit, ordered after it.
  The TAP unit has `ConditionPathExists=` on `net.env`. Without that file the
  unit is skipped, which counts as success, so a VM without `net.env` boots
  with no NIC.

**One nft table, `inet dassie`, default-deny for VM traffic.** Rules match the
`dt*` interface pattern and named sets, never a single VM.

| Flow | Verdict |
| --- | --- |
| VM → VM | Rejected: TCP reset, else ICMP admin-prohibited, so apps fail fast. |
| VM → host (any port, ping included) | Rejected, except replies to connections the host opened. |
| Host → VM | Allowed. Caddy proxies to the guest IP. |
| VM → outside | IPv4 from `@vm_pool` is accepted and masqueraded out whichever uplink the route picks. |
| Outside → VM | Only replies to the VM's own connections. |
| Forged source address | Dropped in prerouting by one `fib saddr . iif oif missing` rule. Otherwise a VM could have replies to another VM's address delivered to that VM. |

Every base chain has `policy accept`, so this table never blocks other
forwarding on the host, such as Docker's. The reverse does not hold. A drop
in any base chain on a hook is final, so a host whose iptables `FORWARD`
policy is `DROP` (Docker sets this) drops VM traffic too. Such a host must
accept `dt*` traffic in iptables, for example in Docker's `DOCKER-USER`
chain. Both firewalls must then accept a packet, so the policy here still
applies. The runbook gives the rules.

**DNS comes from `dassie.dns=<ip>[,<ip>]` on the cmdline (ADR 0005).** Init
writes `/etc/resolv.conf` from it. `net-env.sh` defaults to public resolvers
(`1.1.1.1,9.9.9.9`), because the host's own resolver is often a loopback stub
(systemd-resolved on `127.0.0.53`) that a guest cannot reach.

**Why not a bridge.** A Linux bridge puts every VM in one L2 domain, which is
open VM↔VM unless ebtables or per-port isolation is added, and it is capped at
1024 ports. With routed TAPs the host is the only router, and all policy sits
in one nft table.

**Cardinality.**

- **IFNAMSIZ.** `dt` plus 5 digits leaves room for a later prefix, such as a
  netns tag in M5.
- **Neighbour table.** The host keeps one neighbour entry per running VM. Past
  `gc_thresh3` (Linux default 1024), new entries fail with "neighbour table
  overflow". `dassie-net.service` raises `gc_thresh1/2/3` to
  4096/32768/65536, enough for the whole /16.
- **Rules.** The rule count is fixed. Per-VM or per-app policy goes into set
  elements, which nft looks up by hash or interval, and never into more rules.
- **Addresses.** The /16 holds 32768 /31s.

**Reserved for later milestones (design only, not built in M1):**

- **Peer allow-lists.** Each app gets a named set of its guest addresses, kept
  by the daemon. One approved-pair rule above the VM↔VM reject accepts
  `ip saddr @app_<a> ip daddr @app_<b>` for each approved pair (or a
  concatenated `saddr . daddr` set for many pairs). The rule count scales with
  approvals, not VMs.
- **Stable per-app VIPs.** Each app gets a VIP in `10.89.0.0/16` that is never
  assigned to a TAP. A DNAT map, `ip daddr map @vip_to_live`, sends it to the
  live colour's guest IP. In M4 the map element swaps in the same step as the
  Caddy route, so peers and ingress move together.
- **Per-clone netns (M5).** Snapshot clones share one guest IP and MAC. Each
  clone runs in its own network namespace with an identical TAP inside it. The
  namespace connects to the host through a routed veth with a unique outside
  address, and NAT maps between the two. The host-side policy above then
  applies to the veth as it does to a TAP.

## Consequences

- Isolation holds with no per-VM firewall state. Adding a VM is one `net.env`
  and one TAP.
- Host-to-guest traffic is plain routing, so Caddy and debugging tools such as
  `curl` and `tcpdump -i dt<N>` work against the guest IP directly.
- A VM cannot use any host service, including a local DNS cache or metadata
  endpoint. Anything it needs must come over vsock or from outside.
- IPv6 is out of scope: VMs have no IPv6 connectivity.
- VMs depend on the configured public resolvers being reachable. A host on a
  network that blocks them must set `DASSIE_DNS`.
- Index allocation is manual in M1. Two VMs with the same index fight over the
  same TAP. M2's allocator removes that.
- A guest with root can see its own IP and gateway, but nothing about other
  VMs: there is no shared L2, and anything sent to another VM is rejected.
