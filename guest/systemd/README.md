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

Install them, plus one instance called `test`:

```sh
G=target/guest/x86_64
sudo install -m 0755 $G/firecracker /usr/local/bin/firecracker
sudo install -m 0755 target/release/examples/listener /usr/local/bin/dassie-listener
sudo install -d /var/lib/dassie/guest /var/lib/dassie/vms/test /run/dassie
sudo install -m 0644 $G/vmlinux $G/initramfs.cpio.gz /var/lib/dassie/guest/
sudo install -m 0644 $G/rootfs.ext4 /var/lib/dassie/vms/test/
guest/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/vms/test/rootfs.ext4 /run/dassie/test.vsock "dassie.instance=test" |
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

### Optional: a NIC for drill 1

Drill 1 reaches the testapp over HTTP, so the guest needs a TAP device. The
kernel `ip=` arg sets the guest's address, and `dassie-init` brings `eth0` up:

```sh
sudo ip tuntap add tap0 mode tap
sudo ip addr add 172.16.0.1/24 dev tap0
sudo ip link set tap0 up
FC_TAP=tap0 guest/vm-config.sh /var/lib/dassie/guest/vmlinux /var/lib/dassie/guest/initramfs.cpio.gz \
  /var/lib/dassie/vms/test/rootfs.ext4 /run/dassie/test.vsock \
  "dassie.instance=test ip=172.16.0.2::172.16.0.1:255.255.255.0::eth0:off" |
  sudo tee /var/lib/dassie/vms/test/config.json
sudo systemctl restart fc@test
curl http://172.16.0.2:8080/   # ok
```

`tap0` does not survive a host reboot. Use the config without `FC_TAP` for
drill 4. If the TAP device is missing, Firecracker fails to start and the unit
hits its start limit.

## Drills

Each drill must converge with no manual intervention. To count restarts, run
`systemctl show fc@test -p NRestarts`.

### 1. Kill the entrypoint inside the guest

With the NIC:

```sh
curl http://172.16.0.2:8080/crash
```

`/crash` makes the testapp abort, so it dies of SIGABRT. Without a NIC, put
`dassie.env=EXIT_AFTER=20` into the config's extra boot args instead. The
testapp then exits 0 on its own every 20 s.

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

Use the config without a TAP device.

```sh
sudo systemctl enable fc@test
sudo reboot
```

**Expected:** after the host is back, `systemctl status fc@test` is
`active (running)` via `WantedBy=multi-user.target`. The journal for this boot
shows the guest console. A listener started afterwards still receives
`Started` and `Ready`, because the guest held them. To clean up, run
`sudo systemctl disable --now fc@test`.

### Bonus: the start limit

Set `dassie.env=EXIT_AFTER=1` in the config's extra boot args and restart the
unit.

**Expected:** each boot exits within seconds. After 5 starts in 60 s, the unit
is `failed` with `start-limit-hit`, and it stays down. To recover, fix the
config, then run `sudo systemctl reset-failed fc@test` and
`sudo systemctl start fc@test`.
