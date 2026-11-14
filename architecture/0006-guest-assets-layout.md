# 0006. Guest assets and the initramfs switch-root

Status: Accepted
Date: 2026-11-12

## Context

Booting `dassie-init` as PID 1 in a Firecracker microVM needs a guest kernel, a
way to get init into the guest, an app rootfs, and a unit to run Firecracker.
Issue #2 settled that init ships in one shared initramfs and the app rootfs is
the root drive. ADR 0005 left how init gets from one to the other to the code
that does it. The kernel has to come from a script in the repo, not an opaque
prebuilt binary. It also has to be built the same way for both architectures,
in CI and on a dev Linux box.

An initramfs is a `rootfs` mount, which `pivot_root` cannot leave. It also
starts with no `/proc`, `/sys` or device nodes beyond the kernel's built-in
`/dev/console`.

## Decision

**Layout.** Guest assets live in `scripts/`, as scripts plus the inputs they
need, sharing `scripts/lib.sh`. (Moved from `guest/` on 2026-11-14, #31, to
split guest builds from host tooling and tests. The paths changed; the
decision did not.)

| Path | Builds |
| --- | --- |
| `scripts/guest/kernel/build.sh`, `*.config` | `vmlinux` (x86_64) or `Image` (aarch64) |
| `scripts/guest/initramfs.sh` | `initramfs.cpio.gz`, holding only `/init` = static musl `dassie-init` |
| `scripts/guest/rootfs.sh`, `scripts/guest/rootfs/` | `rootfs.erofs`, the test app image |
| `scripts/host/firecracker.sh` | `firecracker`, the pinned release |
| `scripts/host/vm-config.sh` | a Firecracker `--config-file` for one VM |
| `scripts/test/smoke-test.sh` | the CI boot test |
| `scripts/host/systemd/` | the hand-written `fc@.service` and its runbook |

- Outputs go to `target/guest/<arch>/` (override with `OUT`). Downloads are
  cached in `~/.cache/dassie`.
- Every download is pinned with a sha256 in the script that fetches it:
  - the kernel.org tarball;
  - Firecracker's guest config, at the commit of the Firecracker release we
    run;
  - the Firecracker release itself.
- Scripts build for the host architecture and need no root:
  - `mkfs.erofs` builds the rootfs from a staging directory (`mkfs.ext4 -d`
    until M1 slice C);
  - the cpio holds no device nodes.

**Kernel.** A 6.18 LTS kernel uses Firecracker's
`microvm-kernel-ci-<arch>-6.18.config`, merged with
`scripts/guest/kernel/dassie.config` for both architectures. `build.sh` fails if
`olddefconfig` drops any option in the fragment. The fragment states what init and the host contract rely on:

- initrd (gzip), devtmpfs, proc, sysfs;
- the virtio-mmio transport, virtio-blk, virtio-net and vsock;
- the 8250 serial console;
- `IP_PNP`;
- _amended 2026-11-13, for M1 slice C:_ erofs (ADR 0009) with xattrs, ACLs
  and lz4, overlayfs, and tmpfs with xattrs, so that copy-up keeps them.
  These replace ext4, which the guest no longer needs.

Firecracker's config has no i8042 keyboard, so its x86_64-only Ctrl-Alt-Del
action does not reach the guest. Graceful stop goes through the control
channel's `stop` on both architectures (ADR 0004).

**Boot args.** `console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro`, plus
the per-VM `dassie.*` args and `ip=` when the VM has a NIC.
`is_root_device` is false, so Firecracker adds nothing of its own.

_Amended 2026-11-13, for M1 slice C:_ the root drive is attached with
`is_read_only: true` and the boot args say `ro` (they said `rw` before).
Firecracker run as root ignores a `0444` file mode. init mounts the drive
read-only regardless, but only the drive setting stops a root process in the
guest from writing `/dev/vda`, and so the file every VM shares.

**Switch-root.** `dassie-init` does what busybox `switch_root` does, and stays
PID 1:

1. Mount devtmpfs, proc and sysfs, then reopen `/dev/console` as stdin,
   stdout and stderr.
2. Read `root=` (default `/dev/vda`) and `rootfstype=` (default `erofs`)
   from `/proc/cmdline`. `ro` and `rw` are ignored. Wait up to 5 s for the
   device node.
3. Build the overlay root:
   1. mount the drive read-only on `/lower`;
   2. mount a tmpfs on `/rw` at the kernel's default size (half the guest's
      RAM), and create `/rw/upper` and `/rw/work` in it;
   3. copy the mode, uid and gid of `/lower` onto `/rw/upper`, because the
      merged root takes its attributes from the upper directory, which would
      otherwise be `1777 root`;
   4. mount overlayfs on `/newroot` with `lowerdir=/lower`,
      `upperdir=/rw/upper` and `workdir=/rw/work`.
4. Create `/dev`, `/proc` and `/sys` in the new root if they are missing
   (in the upper layer), and move the three mounts there.
5. `chdir /newroot`, `mount --move . /`, `chroot .`, `chdir /`.

_Amended 2026-11-13, for M1 slice C:_ step 3 used to mount the drive
directly on `/newroot`, read-write unless the cmdline said `ro`. `/lower` and
`/rw` stay mounted under the old initramfs root, out of the app's reach.

After that, init:

- brings `lo` up, and `eth0` if it exists, with the kernel `ip=` arg supplying
  its address;
- disables the Ctrl-Alt-Del reboot, so any Ctrl-Alt-Del arrives as SIGINT;
- runs the supervisor (ADR 0005).

The initramfs is not emptied after the switch. It holds one small binary.

**Exit.** Init reboots the VM in every case. That covers a normal entrypoint
exit and any setup error or panic. Before rebooting it SIGKILLs all other
processes and runs `sync`. It never exits, because PID 1 exiting panics the
kernel.

**Host unit.** `fc@.service` runs `firecracker --no-api --config-file
/var/lib/dassie/vms/%i/config.json`. Shared assets go in
`/var/lib/dassie/guest/`. vsock sockets go in `/run/dassie/`, which outlives
each unit, so a host listener's socket survives VM restarts.

## Consequences

- Updating init means rebuilding one initramfs, not any app image. M1's image
  builder only has to produce a plain filesystem with `/.dassie/image.json`
  (erofs since ADR 0009).
- A kernel or init regression shows up in the `kvm-boot` CI job, which boots
  x86_64. aarch64 is only built and booted by hand, in a local Linux VM.
- CI caches the built x86_64 kernel by the hash of `scripts/guest/kernel/`. A
  cache miss costs a full kernel build.
- The initramfs has no shell or busybox, so a failed boot is debugged from the
  serial console alone. Every init failure is logged there before the reboot.
- `fc@.service` is a template for M2's generated units, not their final form.
  It has no jailer, and it shares the rootfs file with the next boot instead
  of copying it.
- _Amended 2026-11-13, for M1 slice C:_ the rootfs file is never written, so
  every VM of an image shares one file, and every boot starts from the image
  as built. What the app writes to `/` lives in guest RAM and is gone at the
  next boot. M5 snapshots capture it with the rest of guest memory.
- Apps get only `/dev`, `/proc` and `/sys`. There is no `/dev/pts` or `/run`
  tmpfs until an image needs one.
- _Amended 2026-11-13, when M1's first real image (nginx-unprivileged) needed
  it:_ `/dev` also gets what OCI runtimes provide and devtmpfs lacks. That's
  the `/dev/fd` and `/dev/std{in,out,err}` links to `/proc/self/fd`, and a
  `1777` tmpfs on `/dev/shm`, mounted `nosuid,nodev,noexec` and sized by the
  kernel default.
- _Amended 2026-11-13, for M1 slice C:_ the root drive and the overlay add
  no boot time measurable against the ~1.5 s to `ready` (ADR 0009).
