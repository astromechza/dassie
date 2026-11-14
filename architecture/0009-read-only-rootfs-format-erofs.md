# 0009. Read-only rootfs format: erofs

Status: Accepted
Date: 2026-11-13

## Context

ADR 0007 built each image's artefact as ext4. That was only because the
guest still booted the root drive read-write, and it said to revisit the
choice once M1 slice C made the root read-only. Slice C does that: the drive
is attached read-only and is the lower layer of an overlay whose upper layer
is a tmpfs (ADR 0006). The artefact is never written again, so a writable
filesystem buys nothing.

We measured both formats on `docker.io/nginxinc/nginx-unprivileged:alpine`
(linux/arm64, 25 MiB of gzip layers, 2130 entries). Every variant was built
from the same flattened tar stream on stdin. The VM was an aarch64 lima VM
with nested KVM, so the timings only compare variants with each other.

| Variant | Size (apparent / allocated) | mkfs, median of 5 | Boot to `ready`, median of 10 |
| --- | --- | --- | --- |
| ext4 as built by format v2 | 137 / 68 MiB | 141 ms | 1494 ms |
| ext4 tuned: no journal, tight size | 52 / 52 MiB | 181 ms | 1475 ms |
| erofs, uncompressed | 52 / 52 MiB | 86 ms | 1477 ms |
| **erofs, lz4hc** | **31 / 31 MiB** | 889 ms | 1481 ms |
| erofs, lzma | 21 / 21 MiB | 2818 ms | 1675 ms |

Boot to `ready` runs from Firecracker's start to the host listener's `ready`
event. About 0.9 s of that is kernel boot before the root mount. Neither the
overlay nor the erofs-enabled kernel added anything measurable, and guest
memory after `ready` differed by less than 1 MB.

What separates the formats beyond those numbers:

- **Attributes.** mke2fs 1.47.2 with tar input keeps `security.capability`
  but drops every other xattr: other `security.*`, `user.*` and `trusted.*`.
  It also truncates mtimes to whole seconds. `mkfs.erofs --tar=f` keeps all
  of them, with nanosecond mtimes.
- **Sizing.** ext4 needs its size and inode count guessed up front, and a
  wrong guess fails the build (51 MiB failed where 52 MiB worked). erofs is
  sized to its content.
- **Journal.** An ext4 that was ever mounted read-write and not cleanly
  unmounted refuses a read-only mount until its journal is replayed. erofs
  has no journal.
- **Root directory.** With no root entry in the tar, mkfs.erofs makes `/`
  mode `0777`, where mke2fs used `0755`.

## Decision

**Artefacts are erofs, compressed with lz4hc.** The file is
`/var/lib/dassie/rootfs/sha256-<manifest hex>.v3.erofs`. This supersedes
ADR 0007's "Why ext4, for now" section and its mkfs.ext4 build step.
Everything else in ADR 0007 stands: the store, the flattener, the digest
naming, the temporary file plus rename, and `0444`.

- `FORMAT_VERSION` is 3, and `EXTENSION` is `erofs`, which is also the
  `rootfstype=` to boot it with. dassie-init now defaults to `erofs`.
- The build pipes the flattened tar into
  `mkfs.erofs --tar=f -z lz4hc,level=12 -T 1 --mkfs-time -U <uuid> -L dassie`,
  with `SOURCE_DATE_EPOCH` removed from its environment.
  - `-U` takes the UUID from the manifest digest, as before.
  - `-T 1 --mkfs-time` fixes the superblock's build time and nothing else.
    `-T` on its own, or `SOURCE_DATE_EPOCH`, stamps that time on every inode
    and would erase the image's mtimes.
  - With these options the same image builds to the same bytes, across
    working directories, umasks and thread counts.
- The flattener writes an explicit root entry first: `./`, a `0755 root:root`
  directory. Images' own root entries are ignored, as they were before.
- lz4hc boots as fast as uncompressed erofs and is 40% smaller.
  lzma is smaller still but adds ~190 ms to boot and seconds to every build.
- The guest kernel pins `CONFIG_EROFS_FS` with `_XATTR`, `_POSIX_ACL`,
  `_SECURITY` and `_ZIP` (lz4 needs no separate option in 6.18). This adds
  128 KiB to the kernel image. ext4 is no longer part of the guest contract.
- The test app rootfs (`scripts/guest/rootfs.sh`) is erofs too, so CI boots
  the same format as production.

## Consequences

- The host dependency is now erofs-utils 1.8 or later (for `--mkfs-time`).
  Debian 13 ships 1.8.6. e2fsprogs 1.47.1+ and libarchive are no longer
  needed. Ubuntu 24.04 ships 1.7.1, which builds the test rootfs from a
  directory but not reproducible image artefacts. So CI still only
  unit-tests the flattener, and the real build is an ignored test run by
  hand on Linux.
- Images keep every xattr and their full mtimes.
- Artefacts are about a quarter of their old apparent size. That shrinks
  the store and the host page cache that every VM of an image shares. Each
  build costs under a second more, once per image version.
- No size, inode or headroom estimate is left in the builder, and no journal
  can get dirty.
- erofs cannot be mounted read-write, ever. A future need for a writable
  root drive (not the overlay) would need a different format, which the
  format version and extension in the artefact name already allow.
- v2 `.ext4` artefacts are not reused. They stay on disk until GC (M6)
  removes them.
- Tuned ext4 stays the fallback, should erofs ever be unavailable. It
  matches erofs on boot time and nearly on size, but keeps the xattr and
  mtime loss, and its size guess.
