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

**Layout.** Guest assets live in `guest/`, as scripts plus the inputs they
need:

| Path | Builds |
| --- | --- |
| `guest/kernel/build.sh`, `*.config` | `vmlinux` (x86_64) or `Image` (aarch64) |
| `guest/initramfs.sh` | `initramfs.cpio.gz`, holding only `/init` = static musl `dassie-init` |
| `guest/rootfs.sh`, `guest/rootfs/` | `rootfs.ext4`, the test app image |
| `guest/firecracker.sh` | `firecracker`, the pinned release |
| `guest/vm-config.sh` | a Firecracker `--config-file` for one VM |
| `guest/smoke-test.sh` | the CI boot test |
| `guest/systemd/` | the hand-written `fc@.service` and its runbook |

- Outputs go to `target/guest/<arch>/` (override with `OUT`). Downloads are
  cached in `~/.cache/dassie`.
- Every download is pinned with a sha256 in the script that fetches it:
  - the kernel.org tarball;
  - Firecracker's guest config, at the commit of the Firecracker release we
    run;
  - the Firecracker release itself.
- Scripts build for the host architecture and need no root:
  - `mkfs.ext4 -d` builds the rootfs from a staging directory;
  - the cpio holds no device nodes.

**Kernel.** A 6.18 LTS kernel uses Firecracker's
`microvm-kernel-ci-<arch>-6.18.config`, merged with `guest/kernel/dassie.config`
for both architectures. `build.sh` fails if `olddefconfig` drops any option in
the fragment. The fragment states what init and the host contract rely on:

- initrd (gzip), devtmpfs, proc, sysfs;
- the virtio-mmio transport, virtio-blk, virtio-net and vsock;
- ext4 and the 8250 serial console;
- `IP_PNP`.

Firecracker's config has no i8042 keyboard, so its x86_64-only Ctrl-Alt-Del
action does not reach the guest. Graceful stop goes through the control
channel's `stop` on both architectures (ADR 0004).

**Boot args.** `console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw`, plus
the per-VM `dassie.*` args and `ip=` when the VM has a NIC.
`is_root_device` is false, so Firecracker adds nothing of its own.

**Switch-root.** `dassie-init` does what busybox `switch_root` does, and stays
PID 1:

1. Mount devtmpfs, proc and sysfs, then reopen `/dev/console` as stdin,
   stdout and stderr.
2. Read `root=` (default `/dev/vda`), `rootfstype=` (default `ext4`) and
   `ro`/`rw` (default `rw`) from `/proc/cmdline`. Wait up to 5 s for the
   device node, then mount it on `/newroot`.
3. Create `/dev`, `/proc` and `/sys` in the new root if they are missing, and
   move the three mounts there.
4. `chdir /newroot`, `mount --move . /`, `chroot .`, `chdir /`.

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
  builder only has to produce a plain ext4 with `/.dassie/image.json`.
- A kernel or init regression shows up in the `kvm-boot` CI job, which boots
  x86_64. aarch64 is only built and booted by hand, in a local Linux VM.
- CI caches the built x86_64 kernel by the hash of `guest/kernel/`. A cache
  miss costs a full kernel build.
- The initramfs has no shell or busybox, so a failed boot is debugged from the
  serial console alone. Every init failure is logged there before the reboot.
- `fc@.service` is a template for M2's generated units, not their final form.
  It has no jailer, and it shares the rootfs file with the next boot instead
  of copying it.
- Apps get only `/dev`, `/proc` and `/sys`. There is no `/dev/pts`,
  `/dev/shm` or `/run` tmpfs until an image needs one.
