# 0005. Guest init contract

Status: Accepted
Date: 2026-11-11

## Context

Each microVM runs one "container": an app built from an OCI image. Something
has to decide what that means inside the guest: which parts of the image config
apply, where per-VM settings come from, when the app counts as ready, and how
it is stopped. `dassie-init` is PID 1, and the host, the image builder (M1) and
the daemon (M2) all depend on these answers.

An OCI runtime (runc, crun, youki) inside the guest would add namespaces,
cgroups and seccomp on top of the VM. The VM already isolates the app from the
host and from other apps, and each VM runs exactly one app, so a second layer of
isolation buys little. It would still cost a larger guest, a runtime to update,
and more boot-time failure modes.

The guest must reach steady state without the host: after a host reboot or a
daemon crash, systemd starts the VM on its own, and no daemon may be needed to
configure it.

## Decision

**A container is an OCI image used as packaging.** `dassie-init` runs the
image's entrypoint as a plain child process. The VM is the isolation boundary.
There are no namespaces, cgroups, seccomp, capability dropping or OCI runtime
in the guest.

**Config delivery.**

- The image builder writes the image's full OCI config blob, byte for byte, to
  `/.dassie/image.json` in the app rootfs.
- Per-VM values arrive as `dassie.*` kernel cmdline args. Init splits the
  cmdline as the kernel does (whitespace outside double quotes) and ignores
  unknown `dassie.*` keys, so a newer host can pass args an older init does not
  know.

  | Arg | Effect |
  | --- | --- |
  | `dassie.instance=ID` | Sets `DASSIE_INSTANCE` in the app env. |
  | `dassie.colour=C` | Sets `DASSIE_COLOUR` in the app env. |
  | `dassie.env=K=V` (repeatable) | Sets `K=V` in the app env. |
  | `dassie.port=N` | Readiness port. |
  | `dassie.stop_timeout=SECS` | Default grace period between SIGTERM and SIGKILL (10 s). |

- Cmdline values win over the image config.

**Image config fields honoured in M0:**

- `Entrypoint` + `Cmd` form argv. Both empty is an error.
- `Env`: merged by key, with cmdline values replacing image values in place.
  `PATH` defaults to the usual six directories and `HOME` to the user's passwd
  home, else `/`.
- `User`: `name`, `uid`, `name:group` or `uid:gid`, resolved against the
  rootfs `/etc/passwd` and `/etc/group`.
  - A numeric uid without an entry gets gid 0.
  - Supplementary groups are cleared; only the primary gid applies.
  - Empty means root.
- `WorkingDir`: defaults to `/`.
- `ExposedPorts`: only used to choose the readiness port.

Ignored for now: `Volumes`, `StopSignal`, `Labels`, `Healthcheck`, `OnBuild`,
`Shell`, `ArgsEscaped`.

**Readiness.** The readiness port is `dassie.port`, else the lowest `N/tcp` (or
bare `N`) in `ExposedPorts`. If there is one, init polls a TCP connect to
`127.0.0.1:N` every 100 ms and reports `ready` on the first success. With no
port, `ready` follows `started`.

**Supervision.**

- The app gets `/dev/null` as stdin. Its stdout and stderr are init's: the
  serial console, which reaches journald on the host.
- _Amended 2026-11-13:_ when the app runs as a non-root user, init gives it
  the console (`fchown` to its uid:gid) before spawning it, as OCI runtimes
  give a container's stdio to its user. Otherwise the app could not reopen its
  output through `/dev/stdout` or `/dev/stderr` (ADR 0006), which images such
  as nginx do for their logs.
- The app starts with no signals blocked, even though init blocks its own
  stop signals and SIGCHLD to receive them with `sigwait`.
- A stop request sends SIGTERM to the app, then SIGKILL once the timeout passes:
  - a control-channel `stop` (ADR 0004) uses its `timeout_ms`, else the default;
  - SIGTERM or SIGINT to init (Ctrl-Alt-Del) uses the default.
- Later stop requests do not extend the deadline.

**Exit.** When the app exits, init reports `exited` with its code or signal,
waits up to 2 s for delivery, then reboots the VM. Firecracker exits on guest
reboot, and systemd restarts the unit. The exit code is surfaced only through
the `exited` event and the serial console: Firecracker's own exit status does
not carry it.

**Code shape.** The supervisor (spawn, readiness, stop, event reporting, control
requests) owns no process-wide state. Child exits and signals reach it as
messages, so it runs as an ordinary process in tests, as ADR 0003 requires. The
Linux-only PID-1 layer turns signals into those messages and reaps every
child with `waitpid(-1)`. It also does the mounts, chroot and reboot.

**Root filesystem.** _Added 2026-11-13, for M1 slice C:_ the image's files
are read-only. `/` is writable only through an overlay whose upper layer is a
tmpfs (ADR 0006), so:

- writes to `/` live in guest RAM and are discarded at every reboot, which
  includes every entrypoint exit;
- the upper layer is capped at the kernel's tmpfs default, half the guest's
  RAM, and a full one returns `ENOSPC`. `/dev/shm` has its own cap of the
  same size, so together they can still exhaust guest memory;
- nothing persists across boots until volumes exist. The root's mode and
  owner are the image's, not tmpfs's `1777`.

The shared-initramfs and switch-root layout is a separate decision, recorded
in [ADR 0006](0006-guest-assets-layout.md).

## Consequences

- The builder only has to copy the image config. Fields init ignores today
  stay in the rootfs for later milestones.
- A VM reaches steady state with no host involvement: everything it needs is
  in its rootfs and cmdline.
- Apps get no in-guest sandboxing. A compromised app owns its VM, which is
  acceptable because the VM holds nothing else.
- Images expecting `StopSignal`, volumes or a Docker `HEALTHCHECK` do not get
  them yet. Readiness is a TCP connect, not an app-level check.
- The kernel cmdline is readable by the app via `/proc/cmdline`, and env
  overrides travel there, so they must not carry secrets (see ADR 0004).
- Images whose `User` relies on supplementary groups lose those groups.
- _Added 2026-11-13, for M1 slice C:_ an app that keeps state in its root
  filesystem loses it at every restart, as a container without volumes does.
  The writes it makes count against guest RAM.
