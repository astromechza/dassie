# `fc@.service` runbook

[`fc@.service`](fc@.service) runs one Firecracker microVM per instance, with
no daemon. It's hand-written for M0 (issue #2). systemd keeps the VM alive:

1. The entrypoint exits.
2. `dassie-init` reports `exited` and reboots the guest.
3. Firecracker exits 0, and `Restart=always` starts the VM again.

`StartLimitBurst=5` in `StartLimitIntervalSec=60` stops a guest that keeps
exiting from looping forever.

CI runs one part of this automatically: the `kvm-boot` job runs
[`guest/smoke-test.sh`](../smoke-test.sh), which boots the test VM once and
checks for `started`, `ready` and `exited`. The drills below need systemd, and
one of them needs a host reboot, so they run by hand. Use an x86_64 Linux host
with KVM, or an aarch64 Linux VM with nested virtualisation (ADR 0003).
Substitute `aarch64` and `Image` for `x86_64` and `vmlinux` there.

## Install

From the repo root, build the guest assets. They go to `target/guest/x86_64/`
([ADR 0006](../../architecture/0006-guest-assets-layout.md)):

```sh
guest/kernel/build.sh x86_64
guest/initramfs.sh x86_64
guest/rootfs.sh x86_64
guest/firecracker.sh x86_64
cargo build --release -p dassie-proto --example listener
```

Install them, plus one instance called `test`. The rootfs goes in once, read-only,
under `/var/lib/dassie/rootfs/`. Every instance attaches that same file as a
read-only drive, and the guest writes only to a tmpfs overlay
([ADR 0006](../../architecture/0006-guest-assets-layout.md)):

```sh
G=target/guest/x86_64
sudo install -m 0755 $G/firecracker /usr/local/bin/firecracker
sudo install -m 0755 target/release/examples/listener /usr/local/bin/dassie-listener
sudo install -d /var/lib/dassie/guest /var/lib/dassie/rootfs /var/lib/dassie/vms/test /run/dassie
sudo install -m 0644 $G/vmlinux $G/initramfs.cpio.gz /var/lib/dassie/guest/
sudo install -m 0444 $G/rootfs.erofs /var/lib/dassie/rootfs/test.erofs
guest/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/rootfs/test.erofs /run/dassie/test.vsock "dassie.instance=test" |
  sudo tee /var/lib/dassie/vms/test/config.json
sudo install -m 0644 guest/systemd/fc@.service /etc/systemd/system/
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
([ADR 0008](../../architecture/0008-guest-networking.md)). Needs `iproute2`
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

Install the host units, then give `test` index 1 (TAP `dt1`, guest
`10.88.0.3`). The index must be unique among running VMs:

```sh
sudo install -d /etc/dassie /etc/systemd/system/fc@.service.d
sudo install -m 0644 guest/systemd/dassie.nft /etc/dassie/
sudo install -m 0644 guest/systemd/dassie-net.service guest/systemd/dassie-tap@.service /etc/systemd/system/
sudo install -m 0644 guest/systemd/fc@.service.d/net.conf /etc/systemd/system/fc@.service.d/
guest/net-env.sh 1 | sudo tee /var/lib/dassie/vms/test/net.env
FC_NET_ENV=/var/lib/dassie/vms/test/net.env guest/vm-config.sh /var/lib/dassie/guest/vmlinux \
  /var/lib/dassie/guest/initramfs.cpio.gz /var/lib/dassie/rootfs/test.erofs \
  /run/dassie/test.vsock "dassie.instance=test" |
  sudo tee /var/lib/dassie/vms/test/config.json
sudo systemctl daemon-reload
sudo systemctl enable --now dassie-net
sudo systemctl restart fc@test
curl http://10.88.0.3:8080/   # ok
```

- `fc@test` now requires `dassie-tap@test`, which creates `dt1` from
  `net.env` before the VM starts and deletes it when stopped. The TAP stays up
  across VM restarts.
- A VM without `net.env` skips its TAP unit and boots with no NIC.
- `net.env` also carries `DASSIE_DNS` (default `1.1.1.1,9.9.9.9`; set
  `DASSIE_DNS` when running `net-env.sh` to change it), which reaches init as
  `dassie.dns` and becomes the guest's `/etc/resolv.conf`.
- After editing `/etc/dassie/dassie.nft`, run `sudo systemctl reload
  dassie-net`. It swaps the table atomically. A `restart` would also restart
  every TAP and VM.
- `sudo nft list table inet dassie` shows the policy and its counters.

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
`sudo systemctl disable --now fc@test`. With networking, also run
`sudo systemctl stop dassie-tap@test`, because the TAP outlives the VM.

### 5. Two VMs share one rootfs, and writes vanish on restart

Add a second instance on the same rootfs file. Use different vsock sockets
and no NIC, and make it crash every 20 s:

```sh
sudo install -d /var/lib/dassie/vms/test2
guest/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
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
TAP would delete `test`'s live `dt1`, so first run
`sudo systemctl stop fc@test dassie-tap@test`. The testapp's
`/dial?addr=HOST:PORT` opens a TCP connection from inside the guest and
reports the result:

```sh
curl 'http://10.88.0.3:8080/dial?addr=10.88.0.5:8080'   # VM -> VM
curl 'http://10.88.0.3:8080/dial?addr=10.88.0.2:22'     # VM -> host
curl 'http://10.88.0.3:8080/dial?addr=example.com:443'  # egress by name
curl 'http://10.88.0.5:8080/dial?addr=example.com:443'
```

**Expected:**

- VM → VM and VM → host: `error: Connection refused` at once. The forward
  and input chains reject them with a TCP reset.
- Egress: `ok <ip>:443` from both VMs, through the masquerade, with the name
  resolved by the guest from `dassie.dns`.
- `curl http://10.88.0.5:8080/` from the host still answers `ok`.
- Stopping `fc@a` and `dassie-tap@a` removes `dt1`. Starting `fc@a` again
  recreates it, even if a stale `dt1` was left behind.

### Bonus: the start limit

Set `dassie.env=EXIT_AFTER=1` in the config's extra boot args and restart the
unit.

**Expected:** each boot exits within seconds. After 5 starts in 60 s, the unit
is `failed` with `start-limit-hit`, and it stays down. To recover, fix the
config, then run `sudo systemctl reset-failed fc@test` and
`sudo systemctl start fc@test`.
