# `supervise`: the guest contract without a VM

`supervise` runs the real `dassie-init` supervisor as an ordinary process. It
loads config with `config::load`, as PID 1 will, and plays Firecracker's part
for vsock, using Unix sockets. That lets you watch the whole host <-> guest
contract (ADR 0004, ADR 0005) on a laptop, with the `dassie-proto` listener as
the host:

| In a VM | Here |
| --- | --- |
| Rootfs with `/.dassie/image.json` | `--root` directory: [`rootfs/`](rootfs/.dassie/image.json), [`rootfs-stubborn/`](rootfs-stubborn/.dassie/image.json) |
| Kernel cmdline `dassie.*` args | `--cmdline` |
| Guest -> host vsock, forwarded to `<uds_path>_1024` | Unix socket `<uds_path>_1024` |
| Host -> guest vsock via `<uds_path>` + `CONNECT 1025` | `supervise` answers the same handshake on `<uds_path>` |
| SIGTERM/SIGINT to PID 1 | SIGTERM/SIGINT to `supervise` |
| VM reboot on exit | `supervise` exits |

The sample images run host binaries (`python3`, `sh`), so `PATH` and
`WorkingDir` point at host paths.

## Walkthrough

Build once, from the repo root:

```sh
cargo build -p dassie-proto --example listener
cargo build -p dassie-init --example supervise
mkdir -p target/demo
L=target/debug/examples/listener
S=target/debug/examples/supervise
```

**Terminal 1, the host:**

```sh
$L events target/demo/vm.sock
```

**Terminal 2, the guest.** The image serves HTTP on port 8000, and the cmdline
overrides one image env var:

```sh
$S target/demo/vm.sock --root crates/dassie-init/examples/supervise/rootfs \
  --cmdline 'console=ttyS0 dassie.instance=web-a dassie.env="GREETING=from the cmdline"'
```

`supervise` prints the merged `RunSpec`: `GREETING` comes from the cmdline,
`DASSIE_INSTANCE` is injected, and `ready_port` is 8000 from `ExposedPorts`.
Terminal 1 shows `Started`, then `Ready` once the server accepts connections.

**Terminal 3, poking it:**

```sh
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8000/   # 200
$L health target/demo/vm.sock      # Health(HealthStatus { state: Ready, pid: … })
```

**Host restart drill.** Press Ctrl-C in terminal 1, wait, then start it again.
The guest keeps running (`health` still says `Ready`), and later events reach
the new listener.

**Graceful stop:**

```sh
$L stop target/demo/vm.sock --timeout-ms 3000   # Health(... state: Stopping ...)
```

Terminal 1 shows `Exited { status: Signal(15) }`, and `supervise` exits as the
VM would reboot. `kill -TERM <supervise pid>` does the same, as SIGTERM to
PID 1 would.

**SIGKILL escalation.** Run terminal 2 with `--root
crates/dassie-init/examples/supervise/rootfs-stubborn`, an entrypoint that
traps SIGTERM. `stop --timeout-ms 2000` then ends in
`Exited { status: Signal(9) }` about two seconds later.

## Recorded run

From the scripted version of the walkthrough above on macOS:

```text
--- curl
HTTP 200
--- health
Health(HealthStatus { state: Ready, pid: 63404 })
--- kill listener, wait, start a new one
--- health (guest kept running)
Health(HealthStatus { state: Ready, pid: 63404 })
--- stop
Health(HealthStatus { state: Stopping, pid: 63404 })
--- listener 1
[conn 0] Started { pid: 63404 }
[conn 0] Ready
--- listener 2
[conn 0] Exited { status: Signal(15) }
--- stubborn: stop --timeout-ms 2000
Health(HealthStatus { state: Stopping, pid: 63420 })
[conn 0] Started { pid: 63420 }
[conn 0] Ready
[conn 0] Exited { status: Signal(9) }      # 2 s after stop
```
