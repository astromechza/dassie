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
little next to the blob store.

**Why ext4, for now.** ext4 was assumed from the start (ADR 0002's crate
table, then ADR 0006's test rootfs and kernel fragment) rather than chosen
against alternatives. This records the choice:

| | ext4 | erofs | squashfs |
| --- | --- | --- | --- |
| Writable | yes | no | no |
| In the guest kernel | yes, pinned | no: `CONFIG_EROFS_FS` is unset in Firecracker's config | yes, with xattrs |
| Built from a tar on stdin, without root | `mkfs.ext4 -d -` | `mkfs.erofs --tar` | `sqfstar` |
| Compression | none | lz4/lzma, plus dedup | yes; costlier random reads |
| Sized | estimated up front, with a journal and spare metadata | to its content | to its content |
| Reproducible | through the fake-time and UUID options | by design | with options |

ext4 wins for slice A for one reason: until slice C adds the read-only root
plus overlay, the guest boots the root drive read-write, and only ext4 can
do that. The kernel and init already support it, and `debugfs` lets tests
inspect the result.

Once the root is read-only, ext4's advantages mostly go and its costs stay:
no compression, a journal that does nothing, a size guessed in advance, and
`user.*` xattrs dropped. erofs is built for read-only container images. Its
main cost is one kernel option. **Revisit at slice C:** measure ext4 against
erofs on a real image (artefact size, build time, boot to `ready`), and
record the outcome in a new ADR that supersedes this section. A swap is
cheap: the format version and extension in the artefact name keep old and
new files apart, and init already honours `rootfstype=`.

**Scope in M1 slice A.** Only the read side of the store is built. The
registry pull writes blobs and `index.json` (slice B).

_Amended 2026-11-13, when slice B added the write side and the pull:_

- **Write side.** `Store::put` streams a blob into a temporary file in
  `blobs/sha256/` while hashing it, reading at most its descriptor's size
  plus one byte. It renames the file into place only if size and digest
  match, and skips blobs already present. `Store::set_ref` replaces the
  `index.json` entry of one name, writing the file to a temporary name and
  renaming it. It keeps every other entry and field, so it shares a layout
  with `skopeo`. Two concurrent pulls can still lose one name update; M2's
  daemon serializes them.
- **Pull order.** Config, layers, manifest, then the name, so the store
  never names an image it cannot read. Unsupported layers (zstd) and the
  wrong platform are refused before any layer is fetched. Blobs are stored
  byte for byte as served, so a Docker v2 manifest stays Docker v2 and its
  stored digest is the registry's.
- **Client.** Our own sync client over ureq 3 and rustls, with no async
  runtime. It sends anonymous requests and answers the bearer-token
  challenge Docker Hub and GHCR use, renewing the token once per request on
  a 401. Blob redirects to a CDN are followed with the token dropped.
  - Roots come from the host's certificate store through
    `rustls-platform-verifier`, not ureq's bundled `webpki-roots`
    (CDLA-Permissive-2.0, outside the license allow list).
  - The crypto provider is `rustls-graviola`: pure Rust and assembly for
    x86_64 and aarch64, our only targets. ring and aws-lc-rs both compile C
    in their build scripts, which would need a musl C cross toolchain just to
    run clippy on macOS and in CI. It is younger than ring; swapping
    providers later is a one-line change.
  - Timeouts cover connect, response and each read. There are no retries.
  - Bounds: JSON bodies are capped at 4 MiB, a blob at its declared size,
    and a manifest at 128 layers (Docker has long refused more than 127).
    The total image size is not capped yet.
  - `Client::pull_with` reports each blob as present, fetching (bytes so
    far) or stored, for progress display.
- **References** follow Docker's rules. The first component is a registry
  only if it contains `.` or `:` or is `localhost`; otherwise it is
  `docker.io` (API host `registry-1.docker.io`), where a one-component name
  gets `library/`. The tag defaults to `latest`, and a digest overrides a
  tag. Every part is checked against a strict character set before it goes
  into a URL. A pull names the image by the normalized reference
  (`nginx` → `docker.io/library/nginx:latest`) and reports the platform
  manifest's digest, which desired state pins. A reference pinned to a
  manifest the store holds in full resolves without the network.

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
