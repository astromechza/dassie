# 0007. Image store and rootfs build

Status: Accepted
Date: 2026-11-13

## Context

M1 turns an OCI image into the root drive of a microVM. ADR 0006 settled that
the app rootfs is a plain ext4 holding the image's files plus
`/.dassie/image.json`, with nothing of dassie's injected. That leaves three
open questions: where images live on the host, how layers become one
filesystem, and what the artefact is named.

Several constraints shape the answers:

- **Ownership.** Files must keep the numeric uid/gid, modes, device nodes and
  xattrs their layers give them. The guest has no user namespaces (ADR 0005),
  so these ids are exactly the ids the entrypoint runs with.
- **No root on the host.** Unpacking layers into a host directory and running
  `mkfs.ext4 -d <dir>` would lose ownership without root. On macOS it would
  lose everything else too.
- **Sharing.** Every VM of one image version uses the same file: blue, green,
  replicas, and later M5 clones. M1 makes the root drive read-only, with an
  overlay in the guest.
- **Snapshots (M5).** A Firecracker snapshot references its drives by path,
  and the guest page cache in the snapshot holds their contents. A drive must
  never change while a snapshot of it exists.

## Decision

**Store.** Images live in an OCI image layout at `/var/lib/dassie/images/`
(`oci-layout`, `index.json`, `blobs/sha256/<hex>`). It is the layer cache:
layers shared between images are stored once, and a pull fetches only the
missing blobs. `skopeo copy docker://… oci:<store>:<name>` writes the same
format, so offline import needs no dassie code.

- Every read verifies the blob's sha256. A digest must be `sha256:` plus 64
  lowercase hex characters before it is used as a file name.
- An image is named by its `index.json` `ref.name` annotation, or by its
  manifest digest. Indexes are followed to the host platform's manifest.
- The desired state records the manifest digest, not a tag.

**Artefact.** One file per image: `/var/lib/dassie/rootfs/sha256-<manifest
hex>.v<N>.ext4`.

- The file is built once, written to a temporary name and renamed into place,
  then left read-only (`0444`). It is never modified.
- `N` is the builder's `FORMAT_VERSION`. A change that would build different
  bytes from the same image bumps it, so an old artefact is never reused by a
  new builder or overwritten by it.
- Builds are reproducible. The filesystem UUID and directory hash seed come
  from the digest, and `E2FSPROGS_FAKE_TIME` fixes mkfs's own timestamps. The
  same image built on the same host gives the same bytes. The host's
  `mke2fs.conf` defaults still apply, so two hosts can differ.

**Flatten.** Two passes over the layers, bottom to top:

1. Read the headers only, and record which entry wins at each path.
   - A whiteout (`.wh.<name>`) or opaque marker (`.wh..wh..opq`) in a layer
     removes only what lower layers put there, as overlayfs does.
   - A non-directory replacing a directory drops the directory's subtree.
   - Missing parent directories are implied (`0755 root:root`).
   - Paths with `..` are rejected. Image content under `/.dassie` is dropped.
   - A hard link whose target a later layer replaces is an error, rather than
     carrying the old file across layers.
2. Write one tar stream:
   - every winning directory first, sorted, so parents come before children;
   - then each layer again, copying the winning non-directories in layer
     order, which keeps hard link targets ahead of their links;
   - last, `/.dassie/image.json`: the config blob byte for byte (ADR 0005).

   Headers and PAX records are copied as they are, so numeric ids, modes,
   device numbers and `SCHILY.xattr.*` reach the filesystem unchanged.

Layers may be uncompressed or gzip. zstd is rejected for now.

**Build.** The stream goes to `mkfs.ext4 -b 4096 -E root_owner=0:0 -d -` on
stdin.

- The size and inode count come from the first pass, plus headroom.
- This needs e2fsprogs 1.47.1 or later and the libarchive shared library at
  run time. mke2fs reads each entry's numeric ids and xattrs through
  libarchive, and never consults the host's user database.
- mke2fs 1.47.2 keeps `security.*` xattrs from tar input (file capabilities
  such as `cap_net_bind_service` work) but silently drops `user.*` and
  `trusted.*` ones. Few images rely on either. If one does, it needs a newer
  mke2fs or erofs.

**Not one drive per layer.** Stacking per-layer drives with overlayfs in the
guest would share disk between images. It would also hit Firecracker's drive
and MMIO limits on deep images, couple snapshots to many files, and save
little next to the blob store. erofs (`mkfs.erofs --tar`) is left open as a
drop-in for ext4: the root is read-only, and init already honours
`rootfstype=`.

**Scope in M1 slice A.** Only the read side of the store is built. The
registry pull writes blobs and `index.json` (slice B).

## Consequences

- Builds need no root. The guest sees exactly the ownership the image author
  intended, so the app's uid can use the files the build produced.
- `mkfs.ext4` with tar input is a host dependency. Debian 13 ships e2fsprogs
  1.47.2, but `libarchive13t64` must be installed. Ubuntu 24.04's 1.47.0 has no
  tar input, so CI can only unit-test the flattener. The ext4 build itself is
  covered by an ignored test, run by hand on Linux.
- Each layer blob is read and decompressed twice. That's cheap at image sizes,
  and nothing is unpacked to disk.
- An artefact is immutable and named by content. This is what M5 snapshots and
  clones need:
  - GC (M6) must treat snapshots and running VMs as references;
  - the kernel and initramfs must be stored by digest too, before snapshots
    land.
- The artefact's internal ids are never interpreted on the host. M2's jailer
  runs Firecracker as a per-VM uid, so the `0444` artefact has to be linked or
  bind-mounted into each jail.
- Per-layer disk sharing for rootfs files is ruled out. Disk use is one
  flattened file per image version, cleaned up by GC.
