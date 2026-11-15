# 0012. Per-VM runner

Status: Accepted
Date: 2026-11-14

## Context

The guest reports `started`, `ready` and `exited` over vsock as best-effort
events (ADR 0004), and something on the host has to listen on each VM's
`<uds_path>_1024` to receive them. In M0 and M1 that was the listener
example, run by hand. Nothing turned the events into state, so systemd only
learnt that Firecracker exited. It never learnt why, and it never learnt
when the app was ready.

M2's daemon (#11) needs that state. So do M4's jobs (#27), whose result is
the app's exit code. If the daemon listened on every VM's socket, every event
would depend on the daemon being up, and a daemon restart would lose events
mid-boot. systemd already keeps exactly this kind of state, an exit status
and readiness per unit, and it keeps it whether or not dassie is running.

## Decision

**Every VM runs under `dassie vm-run`, the unit's `ExecStart`.** The process
tree is systemd → `fc@<id>.service` → `dassie vm-run` (MainPID) → `firecracker`.
vm-run owns the VM's vsock sockets for exactly one boot. It turns events into
state that systemd keeps, then exits when Firecracker does. The daemon later
reads that state from systemd, and polls `health` (ADR 0004). It never
listens on vsock.

```text
dassie vm-run --config <firecracker config.json> [--firecracker <bin>]
```

- **Before boot:** read the vsock `uds_path` from the config, remove a stale
  `<uds>` and `<uds>_1024` (refusing one that is still live), bind
  `<uds>_1024`, then spawn `firecracker --no-api --config-file <config>`.
  Binding first means the guest's first connect already finds a listener.
- **Events.** One thread accepts on `<uds>_1024` and reads frames until EOF,
  then accepts again. The guest holds one connection at a time.
  - `started`, `ready` and `exited` each become a journald entry, sent over
    the native protocol, with these fields: `MESSAGE`, `PRIORITY` (6, or 4
    for a non-zero code or a signal), `SYSLOG_IDENTIFIER=dassie-vm-run`,
    `DASSIE_EVENT`, `DASSIE_PID`, and `DASSIE_CODE` or `DASSIE_SIGNAL`. The
    journal attaches `_SYSTEMD_UNIT` itself.
  - vm-run writes to the journal only when `$JOURNAL_STREAM` says its stderr
    goes there. Otherwise, or if a send fails, it writes a logfmt line to
    stderr instead, such as `vm-run: event=exited code=3`.
  - `ready` also sends `READY=1` and `STATUS=` on `$NOTIFY_SOCKET`, so a
    `Type=notify` unit becomes `active` when the app is ready, not when
    Firecracker starts. With the variable unset, nothing is sent. Only path
    sockets are supported, which is what the system manager uses. An
    abstract address is logged and skipped.
- **Stop.** On SIGTERM or SIGINT, vm-run sends a control `stop` with the
  guest's default grace period (10 s). It then waits up to 15 s for
  Firecracker, and SIGKILLs it after that. The SIGKILL comes at once only
  when the guest cannot be stopping: vm-run could not connect to its control
  port (the guest is not up yet), or the guest answered with an `error`. A
  connection that closes after the request went out still counts as
  stopping, because a guest whose entrypoint exits on SIGTERM may reboot
  before its answer is written. _Amended 2026-11-15:_ with `--guest`, the
  wait is the guest's `sigterm_timeout_seconds` + 5 s instead of 15 s
  ([ADR 0015](0015-guest-config-drive.md)). A second signal is ignored. The unit uses
  `KillMode=mixed`, so systemd's SIGTERM reaches vm-run alone.
- **Exit status.** One vm-run process is one boot, because the guest reboots
  after `exited` and Firecracker then exits. Its exit status is:

  | Case | Exit |
  | --- | --- |
  | A stop that vm-run was asked for (SIGTERM or SIGINT) | 0 |
  | Firecracker exited 0 after `exited` with `code` N | N |
  | Firecracker exited 0 after `exited` with `signal` N | 128 + N |
  | Firecracker exited 0 with no `exited` event (lost, or a kernel panic) | 196 |
  | The VM could not run: a bad config, a live socket in the way, a spawn error, or Firecracker exiting non-zero or killed | 197 |

  The two reserved codes sit in the gap between 128 + signal (at most 192)
  and systemd's own 200–243. They are clear of sysexits (64–78) and of 126
  and 127 too. vm-run writes the reason for a 196 or 197 to stderr.
- **Drain.** When Firecracker exits, vm-run waits up to 1 s for the events
  thread to reach EOF before it settles the exit status. An `exited` frame
  still being read while the guest reboots is not lost. If the events thread
  has reported no connection at all this boot, vm-run first waits 200 ms for
  one, since a short-lived guest's connection may still be queued. Past the
  1 s, vm-run still reads what is queued, so a queued `exited` sets the
  status, but it reports nothing more.
- **Bounded.** The events thread hands frames to the main loop through a
  queue of 64. When the loop is behind, the thread stops reading and the
  guest's writes wait, so a guest cannot grow vm-run's memory. Each journal
  and notify datagram has a 1 s send timeout, so a stuck journald cannot
  stall a stop.
- vm-run is sync and std-only, apart from `signal-hook` for the signals and
  `clap` for the CLI. It runs no async runtime. The vsock handshake and
  stale-socket handling come from `dassie_vm::vsock`.
- `fc@.service` becomes `Type=notify` with `NotifyAccess=main` and
  `KillMode=mixed`. `ExecStart` is now `dassie vm-run`. The `ExecStartPre`
  socket removal and the `ExecStop` one-liner go. `Restart=always` and the
  start limits stay.

## Consequences

- systemd now holds the VM's state with no daemon running.
  `ExecMainStatus` and `Result` give the app's exit, the unit's `active`
  state means the app is ready, and `journalctl -u fc@<id> DASSIE_EVENT=exited`
  gives the history. `ExecMainStatus` describes only the latest boot:
  with `Restart=always` the next boot replaces it, so #27's jobs need units
  that do not restart, or must read each boot's result from the journal.
- Events are still best effort. A lost `exited` shows up as a 196 rather
  than as a wrong code, and `health` stays the source of truth (ADR 0004).
- An app that exits with 196 or 197 looks like a lost event or a failed VM.
  The journal entry with `DASSIE_CODE` tells them apart.
- An app that never becomes ready holds the unit in `activating` until
  `TimeoutStartSec`, and then the unit fails and restarts. Before, a unit was
  `active` as soon as Firecracker started.
- A guest that ignores `stop` costs at most 15 s, and the unit still stops
  cleanly. It does not hit systemd's `TimeoutStopSec` and end up `failed`.
  The exception is a stalled journald: the `stop` can then wait behind up to
  64 queued events at 1 s each, and systemd's `TimeoutStopSec` kills the
  unit instead.
- vm-run owns `<uds>_1024`, so the listener example's `events` subcommand
  now only works with no VM, as in the `supervise` example. Its `health` and
  `stop` subcommands still work next to vm-run.
- #21's host side (app stdout and stderr over another vsock port into
  journald) lands in vm-run, not the daemon. It needs a second events thread
  on another port and the journal's binary field encoding.
- The jailer slice changes what vm-run spawns and adds a chroot prefix to the
  socket paths. It must keep Firecracker in the foreground with the same
  pid, so vm-run can keep waiting on its child. _Amended 2026-11-14:_ done in
  [ADR 0013](0013-jailed-firecracker.md). The jail's socket is
  `/var/lib/dassie/jail/firecracker/<id>/root/vsock`.
