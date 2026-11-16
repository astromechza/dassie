# Host runbook: `fc@.service`, networking and Caddy

[`fc@.service`](fc@.service) runs one Firecracker microVM per instance, with
no daemon. It's hand-written until M2's daemon renders units. Its main
process is `dassie vm-run`
([ADR 0012](../../../architecture/0012-per-vm-runner.md)), which starts
Firecracker and turns the guest's events into unit state:

- `ready` sends `READY=1`, so the unit is `active` once the app is ready.
- `started`, `ready` and `exited` become journal entries, with
  `DASSIE_EVENT`, `DASSIE_PID` and `DASSIE_CODE` or `DASSIE_SIGNAL` fields.
- Firecracker runs in its jailer
  ([ADR 0013](../../../architecture/0013-jailed-firecracker.md)), as the
  VM's own uid and gid from `jail.env`. Each boot gets a fresh chroot in
  `/var/lib/dassie/jail/firecracker/<name>/root`, with the kernel, initramfs
  and rootfs hard-linked in.
- vm-run exits with the app's exit code (128+N for signal N), so
  `ExecMainStatus` is the app's.
  - It exits 0 after a stop.
  - It exits 196 when no `exited` event arrived.
  - It exits 197 when the VM could not run.

systemd keeps the VM alive:

1. The entrypoint exits.
2. `dassie-init` reports `exited` and reboots the guest.
3. Firecracker exits 0, vm-run exits with the app's code, and `Restart=always`
   starts the VM again.

`StartLimitBurst=5` in `StartLimitIntervalSec=60` stops a guest that keeps
exiting from looping forever.

CI runs one part of this automatically: the `kvm-boot` job runs
[`scripts/test/smoke-test.sh`](../../test/smoke-test.sh). It boots the test
VM through `dassie vm-run` twice, once under `systemd-run`, and checks for
`started`, `ready` and `exited` and for vm-run's exit status. The drills below need systemd, and
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
cargo build --release -p dassie
cargo build --release -p dassie-vm --example listener
```

Install them, plus one instance called `test`, with index 1. The index picks
the VM's jail uid and gid (`jail-env.sh`) and, later, its network. It must be
unique among all VMs. The rootfs goes in once, read-only,
under `/var/lib/dassie/rootfs/`. Every instance attaches that same file as a
read-only drive, and the guest writes only to a tmpfs overlay
([ADR 0006](../../../architecture/0006-guest-assets-layout.md)):

```sh
G=target/guest/x86_64
sudo install -m 0755 $G/firecracker $G/jailer /usr/local/bin/
sudo install -m 0755 target/release/dassie /usr/local/bin/dassie
sudo install -m 0755 target/release/examples/listener /usr/local/bin/dassie-listener
sudo install -d /var/lib/dassie/guest /var/lib/dassie/rootfs /var/lib/dassie/vms/test
sudo install -m 0644 $G/vmlinux $G/initramfs.cpio.gz /var/lib/dassie/guest/
sudo install -m 0444 $G/rootfs.erofs /var/lib/dassie/rootfs/test.erofs
scripts/host/jail-env.sh 1 | sudo tee /var/lib/dassie/vms/test/jail.env

# guest NAME [FIELDS] -> writes the VM's guest.json and builds its guest
# drive (ADR 0015), both 0640 root:<jail gid>. FIELDS are more JSON members,
# such as '"env":{"CRASH_AFTER":"20"}'. With a net.env, eth0's /31 comes
# from it, as deploy-image.sh does. Restart the VM afterwards.
guest() {
  local dir=/var/lib/dassie/vms/$1 ip
  ip=$(sed -n 's/^DASSIE_GUEST_IP=//p' "$dir/net.env" 2>/dev/null)
  printf '{"version":1,"instance_id":"%s"%s%s}\n' "$1" \
    "${ip:+,\"ipv4_address\":\"$ip/31\"}" "${2:+,$2}" |
    sudo tee "$dir/guest.json" | sudo dassie guest-drive --out "$dir/guest.drive" || return
  sudo chown "root:$(sed -n 's/^DASSIE_GID=//p' "$dir/jail.env")" "$dir/guest.json" "$dir/guest.drive"
  sudo chmod 0640 "$dir/guest.json" "$dir/guest.drive"
}
guest test
scripts/host/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/rootfs/test.erofs /var/lib/dassie/vms/test/guest.drive /run/dassie/test.vsock |
  sudo tee /var/lib/dassie/vms/test/config.json
sudo install -m 0644 scripts/host/systemd/fc@.service /etc/systemd/system/
sudo systemctl daemon-reload
```

The guest's per-instance config is its `guest.json`: the instance id (also
its hostname), env, argv overrides, eth0's address and resolvers.
Firecracker attaches the drive built from it as `/dev/vdb`, and
`deploy-image.sh` writes both for you.

Start the VM, and watch its journal in another terminal. The guest's events
are journal entries of their own. Show them with their fields:

```sh
journalctl -fu fc@test       # terminal 1: serial console and events
sudo systemctl start fc@test # terminal 2
journalctl -u fc@test -o verbose --output-fields=MESSAGE,DASSIE_EVENT,DASSIE_PID,DASSIE_CODE,DASSIE_SIGNAL
journalctl -u fc@test DASSIE_EVENT=exited   # only the exits
```

**Expected:**

- `systemctl start` returns only once the guest reports `ready`, about 1.5 s.
  The unit is then `active (running)`, with `Status: "ready"`.
- The journal shows the kernel booting, then
  `dassie-init: starting ["/usr/local/bin/testapp", "image-cmd"]`,
  `testapp: hostname test -> 127.0.1.1` and
  `testapp: listening on port 8080`.
- It also shows `entrypoint started, pid …` and `entrypoint ready` from
  `dassie-vm-run`, with `DASSIE_EVENT=started` and `DASSIE_EVENT=ready`.
- `sudo dassie-listener health /var/lib/dassie/jail/firecracker/test/root/vsock`
  prints `state: Ready`. The listener's `health` and `stop` work next to
  vm-run. Its `events` subcommand does not, because vm-run owns `vsock_1024`
  next to that socket. The `uds_path` in `config.json` does not matter for a
  jailed VM: the socket is always `vsock` in the jail.
- Firecracker runs as the jail uid, 1879048193, in the chroot:
  `ps -o uid,comm -p $(pgrep -P $(systemctl show -p MainPID --value fc@test))`,
  and `sudo ls /proc/<that pid>/root/` lists `kernel`, `initrd`,
  `drive-rootfs`, `config.json`, `dev` and `vsock`. It stays in the unit's
  cgroup (`/proc/<pid>/cgroup`).

**Graceful stop.** `sudo systemctl stop fc@test` returns within a second or
two:

- systemd sends SIGTERM to vm-run alone (`KillMode=mixed`). vm-run sends a
  control-channel `stop`.
- The testapp dies of SIGTERM, and the journal shows
  `entrypoint killed by signal 15` with `DASSIE_SIGNAL=15`.
- The guest reboots, Firecracker exits, and vm-run exits 0. The unit is
  `inactive (dead)`, not `failed`, and is not restarted.
- If the guest ignores the `stop`, vm-run kills Firecracker after 15 s and
  still exits 0.

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

Install the host units, then give `test` its network for index 1 (TAP
`dt00001`, guest `10.88.0.3`). The TAP belongs to the jail uid from
`jail.env`, so only this VM's Firecracker can attach to it:

```sh
sudo install -d /etc/dassie /etc/systemd/system/fc@.service.d
sudo install -m 0644 scripts/host/systemd/dassie.nft /etc/dassie/
sudo install -m 0644 scripts/host/systemd/dassie-net.service scripts/host/systemd/dassie-tap@.service /etc/systemd/system/
sudo install -m 0644 scripts/host/systemd/fc@.service.d/net.conf /etc/systemd/system/fc@.service.d/
scripts/host/net-env.sh 1 | sudo tee /var/lib/dassie/vms/test/net.env
guest test '"nameservers":["1.1.1.1","9.9.9.9"]'
FC_NET_ENV=/var/lib/dassie/vms/test/net.env scripts/host/vm-config.sh /var/lib/dassie/guest/vmlinux \
  /var/lib/dassie/guest/initramfs.cpio.gz /var/lib/dassie/rootfs/test.erofs \
  /var/lib/dassie/vms/test/guest.drive /run/dassie/test.vsock |
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
- `guest` reads `net.env` for eth0's address (`10.88.0.3/31`; init routes
  via `10.88.0.2`), so run it again whenever `net.env` changes. A VM whose NIC
  and `guest.json` disagree fails at boot.
- `net.env` also carries `DASSIE_DNS` (default `1.1.1.1,9.9.9.9`; set
  `DASSIE_DNS` when running `net-env.sh` to change it). `deploy-image.sh`
  copies it into `guest.json`'s `nameservers`, which init writes to the
  guest's `/etc/resolv.conf`; by hand, pass them to `guest` as above.
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

`/crash` makes the testapp abort, so it dies of SIGABRT. Without a NIC, run
`guest test '"env":{"CRASH_AFTER":"20"}'` and restart the unit instead. The
testapp then aborts on its own 20 s after every start.

**Expected:**

- The journal shows `testapp: /crash requested, aborting`, then
  `dassie-init: entrypoint exited with Signal(6)`, then the guest reboot.
- The journal shows `entrypoint killed by signal 6` (`DASSIE_SIGNAL=6`).
  systemd logs `Main process exited, code=exited, status=134/n/a`, then
  `Scheduled restart job`, and the next boot follows, with new `started` and
  `ready` entries.
- `NRestarts` goes up by one.

### 2. Kill Firecracker, or vm-run

Kill Firecracker alone, then vm-run, the unit's main process:

```sh
# The next boot's vm-run, within 30 s. Until the old one notices, the unit
# is still active with the old MainPID, and during RestartSec=1 MainPID is 0.
next_runner() {
  for _ in $(seq 150); do
    pid=$(systemctl show -p MainPID --value fc@test)
    if [ "$pid" -gt 1 ] && [ "$pid" != "$1" ] && systemctl is-active -q fc@test; then
      echo "$pid"
      return
    fi
    sleep 0.2
  done
  systemctl status --no-pager fc@test >&2
  return 1
}
old=$(systemctl show -p MainPID --value fc@test)
sudo kill -KILL $(pgrep -P "$old")
new=$(next_runner "$old") && sudo kill -KILL "$new"
```

**Expected:**

- Firecracker killed: vm-run logs `firecracker failed: signal: 9` and exits
  197. systemd restarts the unit after `RestartSec=1`.
- vm-run killed: systemd SIGKILLs the rest of the unit, Firecracker
  included, at once, and restarts it.
- Either way the new vm-run finds the old boot's sockets stale, rebuilds the
  jail from scratch, and the new boot logs `started` and `ready`.
- `NRestarts` goes up by one each time.

### 3. The app's exit code reaches systemd

Make the testapp exit with code 3 after 5 s, then restart the unit:

```sh
guest test '"env":{"EXIT_AFTER":"5","EXIT_CODE":"3"}'
sudo systemctl restart fc@test
sleep 6
systemctl show fc@test -p ExecMainStatus -p Result -p NRestarts
```

**Expected:**

- The journal shows `entrypoint exited with code 3` with `DASSIE_CODE=3`,
  and systemd logs `Main process exited, code=exited, status=3/NOTIMPLEMENTED`.
- Between boots, `ExecMainStatus=3` and `Result=exit-code`. systemd restarts
  the unit, and `NRestarts` goes up by one per boot. After 5 starts in 60 s
  the unit stops on `start-limit-hit` (see the bonus drill).
- To clean up, regenerate the config without the args, then run
  `sudo systemctl reset-failed fc@test` and `sudo systemctl start fc@test`.

### 4. Reboot the host

```sh
sudo systemctl enable fc@test
sudo reboot
```

With networking, `dassie-net` and `dassie-tap@test` come back too, pulled in
by `fc@test`, and `curl http://10.88.0.3:8080/` answers again.

**Expected:** after the host is back, `systemctl status fc@test` is
`active (running)` via `WantedBy=multi-user.target`. The journal for this boot
shows the guest console, and the `started` and `ready` entries. To clean up, run
`sudo systemctl disable --now fc@test`.

### 5. Two VMs share one rootfs, and writes vanish on restart

Add a second instance on the same rootfs file, with index 3, no NIC, and
make it crash every 20 s:

```sh
sudo install -d /var/lib/dassie/vms/test2
scripts/host/jail-env.sh 3 | sudo tee /var/lib/dassie/vms/test2/jail.env
guest test2 '"env":{"CRASH_AFTER":"20"}'
scripts/host/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/rootfs/test.erofs /var/lib/dassie/vms/test2/guest.drive /run/dassie/test2.vsock |
  sudo tee /var/lib/dassie/vms/test2/config.json
sha256sum /var/lib/dassie/rootfs/test.erofs
sudo systemctl start fc@test fc@test2
# Firecracker is the child of vm-run, the unit's main process.
fc() { pgrep -P "$(systemctl show -p MainPID --value "fc@$1")"; }
sudo ls -l /proc/$(fc test)/fd /proc/$(fc test2)/fd | grep drive-rootfs
# Each jail's drive-rootfs is a hard link to the one file.
sudo stat -c '%i %h %n' /var/lib/dassie/rootfs/test.erofs \
  /var/lib/dassie/jail/firecracker/test{,2}/root/drive-rootfs
```

At startup, testapp creates `/tmp/testapp.boot` on its root filesystem. If
the file is already there, testapp exits 1, because the overlay should have
discarded it.

**Expected:**

- Both VMs reach `Ready`, and both Firecracker processes hold their jail's
  `drive-rootfs` open. All three paths have the same inode: the one
  `test.erofs`.
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
directory, `net.env`, `jail.env`, guest drive and config. `a` reuses `test`'s index, and starting its
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
  resolved by the guest from its `nameservers`.
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
sudo dassie-listener health /var/lib/dassie/jail/firecracker/quiet/root/vsock
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

Run `guest test '"env":{"EXIT_AFTER":"1"}'` and restart the unit.

**Expected:** each boot exits within seconds. After 5 starts in 60 s, the unit
is `failed` with `start-limit-hit`, and it stays down. To recover, fix the
config, then run `sudo systemctl reset-failed fc@test` and
`sudo systemctl start fc@test`.
