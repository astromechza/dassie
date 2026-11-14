# Host runbook: `fc@.service`, networking and Caddy

[`fc@.service`](fc@.service) runs one Firecracker microVM per instance, with
no daemon. It's hand-written for M0 (issue #2). systemd keeps the VM alive:

1. The entrypoint exits.
2. `dassie-init` reports `exited` and reboots the guest.
3. Firecracker exits 0, and `Restart=always` starts the VM again.

`StartLimitBurst=5` in `StartLimitIntervalSec=60` stops a guest that keeps
exiting from looping forever.

CI runs one part of this automatically: the `kvm-boot` job runs
[`scripts/test/smoke-test.sh`](../../test/smoke-test.sh), which boots the test
VM once and checks for `started`, `ready` and `exited`. The drills below need systemd, and
one of them needs a host reboot, so they run by hand. Use an x86_64 Linux host
with KVM, or an aarch64 Linux VM with nested virtualisation (ADR 0003).
Substitute `aarch64` and `Image` for `x86_64` and `vmlinux` there.

## Install

From the repo root, build the guest assets. They go to `target/guest/x86_64/`
([ADR 0006](../../../architecture/0006-guest-assets-layout.md)):

```sh
scripts/guest/kernel/build.sh x86_64
scripts/guest/initramfs.sh x86_64
scripts/guest/rootfs.sh x86_64
scripts/host/firecracker.sh x86_64
cargo build --release -p dassie-vm --example listener
```

Install them, plus one instance called `test`. The rootfs goes in once, read-only,
under `/var/lib/dassie/rootfs/`. Every instance attaches that same file as a
read-only drive, and the guest writes only to a tmpfs overlay
([ADR 0006](../../../architecture/0006-guest-assets-layout.md)):

```sh
G=target/guest/x86_64
sudo install -m 0755 $G/firecracker /usr/local/bin/firecracker
sudo install -m 0755 target/release/examples/listener /usr/local/bin/dassie-listener
sudo install -d /var/lib/dassie/guest /var/lib/dassie/rootfs /var/lib/dassie/vms/test /run/dassie
sudo install -m 0644 $G/vmlinux $G/initramfs.cpio.gz /var/lib/dassie/guest/
sudo install -m 0444 $G/rootfs.erofs /var/lib/dassie/rootfs/test.erofs
scripts/host/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/rootfs/test.erofs /run/dassie/test.vsock "dassie.instance=test" |
  sudo tee /var/lib/dassie/vms/test/config.json
sudo install -m 0644 scripts/host/systemd/fc@.service /etc/systemd/system/
sudo systemctl daemon-reload
```

Run the host listener in its own terminal, then start the VM. You can also
start the listener later, because the guest holds its events until a listener
is there:

```sh
sudo dassie-listener events /run/dassie/test.vsock   # terminal 1
sudo systemctl start fc@test                         # terminal 2
journalctl -fu fc@test                               # terminal 3: serial console
```

**Expected:**

- The journal shows the kernel booting, then
  `dassie-init: starting ["/usr/local/bin/testapp"]` and
  `testapp: listening on port 8080`.
- The listener prints `Started { pid: … }`, then `Ready`.
- `sudo dassie-listener health /run/dassie/test.vsock` prints `state: Ready`.

**Graceful stop.** `sudo systemctl stop fc@test` returns within a second or
two:

- `ExecStop` sends a control-channel `stop`.
- The testapp dies of SIGTERM, and the listener prints
  `Exited { status: Signal(15) }`.
- The guest reboots, and Firecracker exits. systemd does not restart it,
  because this is a manual stop.

### Optional: networking

Drill 1 and the network drill reach the testapp over HTTP, so the guest needs
a NIC. Each VM gets a routed TAP and a /31 from `10.88.0.0/16`, and one nft
table holds the policy for all of them
([ADR 0008](../../../architecture/0008-guest-networking.md)). Needs `iproute2`
and `nftables` on the host.

Every other firewall on the host must also let forwarded VM traffic, i.e.
egress, through: a drop in any forward chain is final, whatever `dassie.nft`
says. Host → VM connections use the input and output hooks and are unaffected. Check with
`sudo iptables -S FORWARD`. If the policy is `-P FORWARD DROP`, accept the
TAPs in iptables.

The inserts below go to the top of their chain, so they bypass any DROP or
REJECT already there for `dt+` traffic. First list the chain with
`sudo iptables -S DOCKER-USER` or `sudo iptables -S FORWARD`. Re-insert above
the accepts any rule that must still restrict guest egress.

On a Docker host, which sets that policy, use Docker's `DOCKER-USER` chain:

```sh
sudo iptables -I DOCKER-USER -i dt+ -j ACCEPT
sudo iptables -I DOCKER-USER -o dt+ -j ACCEPT
```

On a host without Docker, add the same rules to `FORWARD`:

```sh
sudo iptables -I FORWARD -i dt+ -j ACCEPT
sudo iptables -I FORWARD -o dt+ -j ACCEPT
```

The `inet dassie` table still rejects VM → VM and VM → host traffic and
drops unsolicited traffic into VMs, because both firewalls must accept a
packet. Egress, though, is then filtered by `dassie.nft` alone.

These rules do not survive a reboot. Persist them the way the host manages
iptables, for example with `iptables-persistent`.

Install the host units, then give `test` index 1 (TAP `dt00001`, guest
`10.88.0.3`). The index must be unique among running VMs:

```sh
sudo install -d /etc/dassie /etc/systemd/system/fc@.service.d
sudo install -m 0644 scripts/host/systemd/dassie.nft /etc/dassie/
sudo install -m 0644 scripts/host/systemd/dassie-net.service scripts/host/systemd/dassie-tap@.service /etc/systemd/system/
sudo install -m 0644 scripts/host/systemd/fc@.service.d/net.conf /etc/systemd/system/fc@.service.d/
scripts/host/net-env.sh 1 | sudo tee /var/lib/dassie/vms/test/net.env
FC_NET_ENV=/var/lib/dassie/vms/test/net.env scripts/host/vm-config.sh /var/lib/dassie/guest/vmlinux \
  /var/lib/dassie/guest/initramfs.cpio.gz /var/lib/dassie/rootfs/test.erofs \
  /run/dassie/test.vsock "dassie.instance=test" |
  sudo tee /var/lib/dassie/vms/test/config.json
sudo systemctl daemon-reload
sudo systemctl enable --now dassie-net
sudo systemctl restart fc@test
curl http://10.88.0.3:8080/   # ok
```

- `fc@test` now requires `dassie-tap@test`, which creates `dt00001` from
  `net.env` before the VM starts. The TAP stays up across VM restarts, and
  goes away when `fc@test` stops.
- A VM without `net.env` skips its TAP unit and boots with no NIC, even if
  `dassie-net` has failed. A networked VM does not start unless the
  `inet dassie` table is loaded.
- VMs cannot reach link-local (cloud metadata), RFC 1918 or CGNAT addresses:
  the `no_egress` set in `dassie.nft`. To use a resolver on a private network
  in `DASSIE_DNS`, remove its range there and reload.
- VMs get addresses from `10.88.0.0/16`. If the host's LAN or a VPN already
  uses that range, pick another /16. Set `DASSIE_POOL` (e.g. `172.31`) for
  every `net-env.sh` run, and change the `vm_pool` element in
  `/etc/dassie/dassie.nft` to match (`172.31.0.0/16`), then reload.
- `net.env` also carries `DASSIE_DNS` (default `1.1.1.1,9.9.9.9`; set
  `DASSIE_DNS` when running `net-env.sh` to change it), which reaches init as
  `dassie.dns` and becomes the guest's `/etc/resolv.conf`.
- After editing `/etc/dassie/dassie.nft`, run `sudo systemctl reload
  dassie-net`. It swaps the table atomically. A `restart` would also restart
  every TAP and VM.
- `sudo nft list table inet dassie` shows the policy and its counters.

### Optional: ingress, and deploying an image

Caddy on the host serves `https://HOSTNAME/` for each VM that has a route,
from its own local CA
([ADR 0010](../../../architecture/0010-ingress-through-caddy.md)).
`scripts/host/deploy-image.sh` goes from an image reference to such a VM in one
command. Set up networking as above first. It also needs a Rust toolchain
and erofs-utils ≥ 1.8 for `dassie-image`.

`deploy-image.sh` is M1 scaffolding: M2's daemon replaces it, along with the
per-VM `route.env` and the hand-picked index. What stays is
[`caddy.service`](caddy.service) and the shape of `/etc/dassie/caddy.json`.

Install Caddy and its unit. `deploy-image.sh` writes `/etc/dassie/caddy.json`,
then enables and starts Caddy:

```sh
scripts/host/caddy.sh x86_64
sudo install -m 0755 target/guest/x86_64/caddy /usr/local/bin/caddy
sudo install -m 0644 scripts/host/systemd/caddy.service /etc/systemd/system/
sudo systemctl daemon-reload
```

Deploy an image. The arguments are the reference, a VM name, an index (unique
per VM, as for `net-env.sh`), and optionally a hostname and the guest port
(default 80). Run it as your user, from the checkout; it uses sudo where it
needs root:

```sh
scripts/host/deploy-image.sh docker.io/traefik/whoami:latest web 10 web.localhost
```

It pulls the image and builds its rootfs, then writes
`/var/lib/dassie/vms/web/{net.env,config.json,route.env}`. It enables and
starts `fc@web` and renders `/etc/dassie/caddy.json` from every VM's
`route.env`. A file is only replaced when its content changes, so redeploying
changes nothing unless the reference now resolves to another digest. Without
a hostname the VM gets no route.

Caddy's CA is not in the host's trust store. Give it to clients explicitly.
curl resolves `*.localhost` to the loopback address by itself. For other
names, use `/etc/hosts` or `--resolve web.example:443:127.0.0.1`:

```sh
sudo cat /var/lib/caddy/pki/authorities/local/root.crt > caddy-root.crt
curl --cacert caddy-root.crt https://web.localhost/
```

- `/etc/dassie/caddy.json` is the whole config. `persist: false` stops Caddy
  keeping a copy of its own, so a restart or reboot loads exactly that file.
- After editing it by hand, run `sudo systemctl reload caddy`. It loads the
  file through the admin API on `/run/caddy/admin.sock`, and keeps the old
  config if the new one fails.
- The admin API listens only on that socket, never on TCP. Caddy still logs
  `admin endpoint on open interface; host checking disabled` for it: host
  checking is for TCP listeners, and the warning is harmless here.
- A new hostname's certificate is issued just after the reload. A request in
  the same second may fail with a TLS `internal error` alert; retry.
- If Caddy refuses a new config, `deploy-image.sh` puts the previous
  `caddy.json` back and fails, so the file always says what Caddy runs.
  Fix the cause and run the deploy again. A hostname that another VM
  already has is refused before any file changes.
- Caddy runs as a dynamic user. Its CA and certificates live in
  `/var/lib/caddy`, a link into `/var/lib/private`.
- `deploy-image.sh` does not lock. Run one deploy at a time.
- To remove an app, `sudo systemctl disable --now fc@NAME`, delete
  `/var/lib/dassie/vms/NAME`, and redeploy any other app (or edit
  `caddy.json` and reload) to drop its route.

## Drills

Each drill must converge with no manual intervention. To count restarts, run
`systemctl show fc@test -p NRestarts`.

### 1. Kill the entrypoint inside the guest

With the NIC:

```sh
curl http://10.88.0.3:8080/crash
```

`/crash` makes the testapp abort, so it dies of SIGABRT. Without a NIC, put
`dassie.env=CRASH_AFTER=20` into the config's extra boot args instead. The
testapp then aborts on its own 20 s after every start.

**Expected:**

- The journal shows `testapp: /crash requested, aborting`, then
  `dassie-init: entrypoint exited with Signal(6)`, then the guest reboot.
- systemd logs `Scheduled restart job`, and the next boot follows.
- The listener prints `Exited { status: Signal(6) }`, then, on a new
  connection, `Started` and `Ready`.
- `NRestarts` goes up by one.

### 2. Kill the Firecracker process

```sh
sudo systemctl kill -s KILL fc@test
```

**Expected:**

- systemd sees the main process die of SIGKILL and restarts the unit after
  `RestartSec=1`.
- `ExecStartPre` removes the stale `/run/dassie/test.vsock`, so the new
  Firecracker can bind it.
- The listener sees the old connection close with no `exited`, because the
  VM vanished. Then it sees a new `Started` and `Ready`.
- `NRestarts` goes up by one.

### 3. Kill the host listener mid-message

Kill the listener with Ctrl-C or `kill -9` while the VM runs. Then restart the
VM so the guest has events to send while no listener is up:

```sh
sudo systemctl restart fc@test
sudo dassie-listener health /run/dassie/test.vsock   # Ready
sudo dassie-listener events /run/dassie/test.vsock   # start it again after a few seconds
```

**Expected:**

- The guest keeps running: `health` answers `Ready` throughout, the journal
  shows no reboot, and `NRestarts` does not change.
- Events the guest could not deliver stay pending (ADR 0004). Within about
  500 ms of the listener coming back, it receives `Started` and `Ready` from
  this boot.
- An event that was written into a connection the listener then dropped
  unread is lost, which is expected. `health` is the source of truth.
- For the "mid-message" case, kill the listener while a crash from drill 1 is
  being reported. The `exited` may be lost, but the VM still reboots and comes
  back, and the next listener sees the new `Started`.

### 4. Reboot the host

```sh
sudo systemctl enable fc@test
sudo reboot
```

With networking, `dassie-net` and `dassie-tap@test` come back too, pulled in
by `fc@test`, and `curl http://10.88.0.3:8080/` answers again.

**Expected:** after the host is back, `systemctl status fc@test` is
`active (running)` via `WantedBy=multi-user.target`. The journal for this boot
shows the guest console. A listener started afterwards still receives
`Started` and `Ready`, because the guest held them. To clean up, run
`sudo systemctl disable --now fc@test`.

### 5. Two VMs share one rootfs, and writes vanish on restart

Add a second instance on the same rootfs file. Use different vsock sockets
and no NIC, and make it crash every 20 s:

```sh
sudo install -d /var/lib/dassie/vms/test2
scripts/host/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/rootfs/test.erofs /run/dassie/test2.vsock \
  "dassie.instance=test2 dassie.env=CRASH_AFTER=20" |
  sudo tee /var/lib/dassie/vms/test2/config.json
sha256sum /var/lib/dassie/rootfs/test.erofs
sudo systemctl start fc@test fc@test2
sudo ls -l /proc/$(systemctl show -p MainPID --value fc@test)/fd \
  /proc/$(systemctl show -p MainPID --value fc@test2)/fd | grep test.erofs
```

At startup, testapp creates `/tmp/testapp.boot` on its root filesystem. If
the file is already there, testapp exits 1, because the overlay should have
discarded it.

**Expected:**

- Both VMs reach `Ready`, and both Firecracker processes hold the same
  `test.erofs` open.
- Each boot of `fc@test2` logs `testapp: / is 0755 root` and
  `testapp: clean root, write ok`. After `CRASH_AFTER=20 elapsed, aborting`
  and the restart, the next boot logs `clean root` again, not `stale write
  from a previous boot`.
- `sha256sum` of the rootfs is unchanged after any number of restarts.
- To clean up, run `sudo systemctl stop fc@test2`.

Leave `is_read_only: true` in the config, even though the file is `0444`.
Firecracker run as root opens a drive read-write unless the config says
otherwise. dassie-init mounts it read-only either way, but a root process in
the guest could still write `/dev/vda` directly, and so the file every VM
shares.

### 6. Network isolation and egress

Two networked VMs, `a` (index 1, `10.88.0.3`) and `b` (index 2, `10.88.0.5`).
Set them up as for `test` above. They share `test.erofs`, and each has its own
directory, `net.env` and config. `a` reuses `test`'s index, and starting its
TAP would delete `test`'s live `dt00001`, so first run
`sudo systemctl stop fc@test`, which also removes its TAP. The testapp's
`/dial?addr=HOST:PORT` opens a TCP connection from inside the guest and
reports the result:

```sh
curl 'http://10.88.0.3:8080/dial?addr=10.88.0.5:8080'   # VM -> VM
curl 'http://10.88.0.3:8080/dial?addr=10.88.0.2:22'     # VM -> host
curl 'http://10.88.0.3:8080/dial?addr=169.254.169.254:80'  # metadata
curl 'http://10.88.0.3:8080/dial?addr=example.com:443'  # egress by name
curl 'http://10.88.0.5:8080/dial?addr=example.com:443'
```

**Expected:**

- VM → VM, VM → host and VM → metadata: `error: Host is unreachable` at
  once. The forward and input chains reject them with ICMP admin-prohibited.
- Egress: `ok <ip>:443` from both VMs, through the masquerade, with the name
  resolved by the guest from `dassie.dns`.
- `curl http://10.88.0.5:8080/` from the host still answers `ok`.
- Stopping `fc@a` removes `dt00001`. Starting it again recreates it, even if a
  stale `dt00001` was left behind.

### 7. Deploy an image by reference

```sh
scripts/host/deploy-image.sh docker.io/traefik/whoami:latest web 10 web.localhost
curl --cacert caddy-root.crt https://web.localhost/
curl -I http://web.localhost/
```

**Expected:**

- The deploy prints `fetched 4 of 4 blobs`, the rootfs path, `(re)started`
  and `caddy: reloaded`.
- `curl` over HTTPS prints whoami's page, served by the VM: `Hostname:
  10.88.0.21`, and `RemoteAddr: 10.88.0.20:…`, the host end of `dt00010`, which
  is Caddy.
- Plain HTTP answers `308` to `https://web.localhost/`.

### 8. Redeploy the same reference

Run the same deploy again. Before and after, record
`systemctl show fc@web -p NRestarts -p MainPID -p ActiveEnterTimestamp`,
`systemctl show caddy -p MainPID`, and the mtimes of the rootfs, the VM's
files and `caddy.json`.

**Expected:** `all 4 blobs already present, fetched nothing`;
`unchanged, not restarted`; `caddy: unchanged, not reloaded`. Everything
recorded is identical. A tag that moved to a new digest would instead pull,
build a new rootfs and restart the VM.

### 9. A VM with no route

```sh
scripts/host/deploy-image.sh docker.io/nginxinc/nginx-unprivileged:alpine quiet 11
sudo dassie-listener health /run/dassie/quiet.vsock
```

**Expected:** `route: none`, and `caddy.json` has no `vm-quiet` route.
`fc@quiet` is active, and `health` prints `state: Ready`: readiness travels
over vsock, from the port in the image's `ExposedPorts` (ADR 0005), whatever
Caddy does.

### 10. Power-cycle the host

With `web` and `quiet` deployed, cut the power: `limactl stop --force` on a
lima VM, or the hypervisor's power-off. Do it within a few seconds of a deploy
too, to check that its files were written durably. Then boot it.

**Expected:** with no manual step, `dassie-net`, `dassie-tap@web`,
`dassie-tap@quiet`, `fc@web`, `fc@quiet` and `caddy` are all active.
`sudo nft list table inet dassie` shows the table. `dt00010` and `dt00011`
are up. `curl --cacert caddy-root.crt https://web.localhost/` answers, and
`health` is `Ready` for both VMs.

### Bonus: the start limit

Set `dassie.env=EXIT_AFTER=1` in the config's extra boot args and restart the
unit.

**Expected:** each boot exits within seconds. After 5 starts in 60 s, the unit
is `failed` with `start-limit-hit`, and it stays down. To recover, fix the
config, then run `sudo systemctl reset-failed fc@test` and
`sudo systemctl start fc@test`.
