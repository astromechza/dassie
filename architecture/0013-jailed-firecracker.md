# 0013. Firecracker runs in the jailer

Status: Accepted
Date: 2026-11-14

## Context

Firecracker runs as root under `dassie vm-run` (ADR 0012). A guest escape
into the VMM would then be root on the host, with every other VM's sockets
and files in reach. Firecracker's own `jailer` is the supported fix. It
gives each VM a chroot, a uid/gid of its own, a mount namespace and Firecracker's
seccomp filters, then execs Firecracker in place. The jailer ships in the
release tarball we already pin (`FC_VERSION=v1.17.0`).

The jailer's behaviour, read from its v1.17.0 source (`src/jailer/src`) and
checked by hand on the lima VM, shapes how vm-run uses it:

- **Chroot.** The chroot is `<chroot-base>/<exec file name>/<id>/root`. The
  jailer creates it with `create_dir_all`, copies Firecracker into it, mknods
  `/dev/kvm`, `/dev/net/tun`, `/dev/userfaultfd` and `/dev/urandom` (0600,
  owned by the jail uid), and chowns `/`, `/dev`, `/dev/net` and `/run` to
  the uid at 0700. It then bind-mounts the chroot onto itself in a new
  mount namespace and calls `pivot_root`.
- **Leftovers are fatal.** The mknods fail on `EEXIST`, and the pid file is
  created with `create_new`. A chroot a crash left behind therefore fails the
  next start.
- **Paths are inside the jail.** Every path Firecracker opens is resolved
  inside the jail: `--config-file`, the kernel, the initrd, the drives and
  the vsock `uds_path`.
- **Process.** Without `--daemonize` or `--new-pid-ns` the jailer stays in
  the foreground and execs Firecracker with the same pid. Its exit status is
  Firecracker's.
- **Process environment.** The jailer closes fds 3 and up, clears the
  environment, and sets `RLIMIT_NOFILE` to 2048.
- **cgroups.** With no `--cgroup` flags and the default
  `--cgroup-version 1`, the jailer does not touch cgroups at all. With
  `--cgroup-version 2` it moves itself into `<cgroup root>/<parent-cgroup>`
  if that directory exists, which would take the VM out of its systemd unit.
- **`--id`** must be 1–64 characters, alphanumeric or `-`.
- **Network.** `--netns` is optional. Without it, Firecracker opens the TAP
  by name in the host netns.

## Decision

**Every VM's Firecracker runs under the jailer, started by vm-run.**

```text
dassie vm-run --config <config.json> --id <id> --uid <uid> --gid <gid>
              [--jailer jailer] [--firecracker firecracker]
              [--chroot-base /var/lib/dassie/jail]
```

- **The chroot.** It lives under `/var/lib/dassie/jail` and is
  `/var/lib/dassie/jail/firecracker/<id>/root`. That is the same filesystem as
  `/var/lib/dassie/{guest,rootfs}`, so assets can be hard-linked. `/run` is a
  tmpfs and cannot hold the links.
- **A fresh chroot every boot.** Before each boot vm-run checks that the
  previous boot's vsock sockets are stale, as in ADR 0012. It then removes
  `/var/lib/dassie/jail/firecracker/<id>` and builds the chroot again. A
  crash leaves nothing for the next start to trip on. The jailer's mounts
  live in its own namespace, so the host sees only plain files there. The
  chroot of a stopped VM stays until its next start, for debugging.
- **Assets are hard-linked, not bind-mounted.** vm-run reads the host-path
  config (as `vm-config.sh` writes it) and links each file into the jail
  root under a fixed name:

  | Host config key | In the jail |
  | --- | --- |
  | `boot-source.kernel_image_path` | `/kernel` |
  | `boot-source.initrd_path` | `/initrd` |
  | each `drives[].path_on_host` | `/drive-<drive_id>` |
  | `vsock.uds_path` | `/vsock` |

  It writes the rewritten config to `/config.json` in the jail, and starts
  `jailer --id <id> --exec-file <firecracker> --uid --gid --chroot-base-dir
  /var/lib/dassie/jail -- --no-api --config-file /config.json`.
  - A hard link needs no cleanup and no mount, and it keeps the shared
    `0444` rootfs one file (ADR 0006, ADR 0007). Bind mounts would need
    unmounting before the chroot can be removed.
  - A link across filesystems fails (`EXDEV`), and vm-run exits 197 with
    that error. It does not fall back to a copy.
  - The files keep their root ownership and modes (0644 assets, the 0444
    rootfs). The jail uid can read them but not write them, and the drive
    stays `is_read_only`.
- **Stable socket paths.** The VM's vsock is always
  `/var/lib/dassie/jail/firecracker/<id>/root/vsock` on the host, whatever
  `uds_path` the host config names. vm-run binds `vsock_1024` next to it
  before the jailer runs, and chowns it to the jail uid so Firecracker can
  connect. The daemon (ADR 0012) and `dassie-listener health|stop` use the
  `vsock` path.
- **The uid and gid** are inputs for now. They come from the instance's
  `/var/lib/dassie/vms/<id>/jail.env` (`DASSIE_UID`, `DASSIE_GID`), and the
  daemon will allocate them later. The hand-written range is
  `0x70000000 + index` (1879048192 and up), for uid and gid alike. That range
  is above systemd's container range (524288–1879048191), which is where
  rootless containers' subuids usually sit, and below the upper half of the
  uid space, which is to be avoided. The uid needs no passwd entry.
- **No cgroup flags.** vm-run passes no `--cgroup` and no
  `--cgroup-version`, so Firecracker stays in the unit's cgroup. Resource
  limits belong to systemd (`MemoryMax=`, `CPUQuota=` and so on), and the
  daemon will render them. `KillMode=mixed` keeps working.
- **No `--daemonize`, `--new-pid-ns` or `--netns`.** vm-run waits on its
  child, which is Firecracker after the exec. The TAP stays in the host netns
  (ADR 0008).
- **Device nodes** are the jailer's own mknods. The host's `/dev/kvm`
  permissions do not matter.
- **The TAP is owned by the jail uid.** `dassie-tap@` creates it with
  `ip tuntap add … user $DASSIE_UID group $DASSIE_GID` from the same
  `jail.env`. Another uid, such as another VM's, cannot attach to it.
- **`fc@.service` always jails.** Its `ExecStart` passes `--id %i` and the
  uid and gid from `EnvironmentFile=/var/lib/dassie/vms/%i/jail.env`, so a
  VM without a `jail.env` does not start. Running vm-run without `--uid` is
  still supported for development and the smoke test's non-root boot, where
  there is no root to run the jailer.

## Consequences

- A guest that escapes into Firecracker gets an unprivileged uid of its own.
  It is in an empty chroot with no `/proc`, under seccomp, and can reach only
  its own files and devices.
- Every boot copies the Firecracker binary into the chroot (3.4 MB) and
  makes a few hard links. Neither adds measurable time to the ~1.5 s boot.
- Every instance needs a uid. Until the daemon allocates them,
  `deploy-image.sh` and the runbook derive one from the instance's network
  index, so the index must stay unique across all VMs, not only networked
  ones.
- Guest assets must live on the chroot base's filesystem. Moving
  `/var/lib/dassie/jail` elsewhere means moving the store with it.
- The vsock path in a host config is now ignored by the units, and only the
  jail path counts. Unjailed runs (development, the non-root smoke boot)
  still use it.
- Resource limits are systemd's, not the jailer's. The jailer's own
  `RLIMIT_NOFILE` of 2048 applies regardless.
- `PR_SET_PDEATHSIG` would not survive the jailer's setuid, but nothing
  needs it: systemd kills the unit's cgroup when vm-run dies (ADR 0012).
