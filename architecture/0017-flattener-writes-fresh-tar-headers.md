# 0017. The flattener writes fresh tar headers

Status: Accepted
Date: 2026-11-18

## Context

ADR 0007 has the flattener copy each winning entry's header and PAX
records as they are, so numeric ids, modes, device numbers and xattrs reach
the filesystem unchanged. The flattener checks a layer as the `tar` crate
parses it, but the filesystem is whatever `mkfs.erofs --tar=f` (ADR 0009)
parses from the same bytes. Every header field the two read differently is a
place where the rootfs can differ from the tree dassie checked: whiteouts,
`/.dassie` and the hard link checks all assume the `tar` crate's reading.

Copying meant every field dassie never looked at went through verbatim, and
the fixes came one field at a time: GNU atime/ctime read as a path prefix, a
stale ustar prefix applied twice, unrecognised extension headers, and NUL
bytes in names. The fuzz oracle could not catch any of them, because it
parsed the output with the same crate that wrote it.

A differential oracle (the `flatten_erofs` fuzz target) now builds each
accepted output with the real mkfs.erofs and loop-mounts it. On a short
`flatten_bytes` corpus, 166 of 698 inputs diverged with copied headers:

- v7 headers and headers with a mangled magic, copied verbatim, which
  mkfs.erofs refuses ("invalid tar magic", "chksum mismatch"), so the build
  fails;
- PAX and GNU long-name headers the `tar` crate re-reads from the output as
  file entries, and mkfs.erofs does not;
- xattrs the kernel lists but will not read (a malformed
  `security.capability`).

## Decision

The flattener writes one fresh GNU header for every output entry and copies
nothing else from the input header.

- **Fields:** type, path, link target, permission bits (`& 0o7777`),
  numeric uid and gid, mtime, size, and device numbers for character and
  block devices. Long names and link targets go in `././@LongLink` entries,
  never a ustar prefix. Numbers too big for octal go in base-256.
- **PAX `uid`, `gid` and `mtime`** replace the header fields, as POSIX
  says, and are written back as header fields. uid and gid must fit 32 bits
  (erofs's limit). mtime keeps whole seconds, and times before the epoch
  become 0. A field that does not parse is an error, except an empty
  (all-NUL or all-space) field, which GNU tar and mkfs.erofs read as 0.
- **xattrs:** only `SCHILY.xattr.*` records in the namespaces erofs stores
  (`user.`, `trusted.`, `security.`, `system.posix_acl_*`). Others, such as
  macOS's `com.apple.*`, are dropped, as mkfs.erofs would drop them. A
  `security.capability` that is not a revision 1, 2 or 3 `vfs_cap_data` is
  dropped too, since the kernel refuses to read it. Every other PAX record
  (`uname`, `gname`, `atime`, `ctime`, `LIBARCHIVE.xattr.*`, `GNU.sparse.*`,
  …) is dropped.
- **Entry types:** regular (`0`, NUL, contiguous `7`), directory, symlink,
  hard link, character and block devices, and FIFOs. Any other type is an
  error (`Unsupported`). Contiguous files are written as regular files.
- A hard link's header carries nothing but its path and target: the inode's
  metadata comes from its target.

`flatten_erofs` runs in CI over the corpus the `flatten_bytes` job grows.

## Consequences

- mkfs.erofs only ever reads fields dassie wrote. Any odd encoding in an
  input header is either parsed (and then re-encoded in the one form) or
  rejected, instead of reaching mkfs.erofs.
- `clear_prefix` and the field-by-field fixes go away. The NUL and `..`
  checks on names stay: a fresh header can still carry those bytes.
- Some images that built before now fail with `BadField` or `Unsupported`:
  a uid above 2³²−1, an unparseable numeric field, or a GNU volume or
  multi-volume entry. None of these could have produced a faithful rootfs.
- Some metadata is dropped on purpose: user and group names (erofs stores
  numeric ids only), sub-second mtimes, and xattrs in namespaces erofs does
  not store or in libarchive's encoding. Images built by Docker, BuildKit
  and other Go `archive/tar` writers use `SCHILY.xattr.*`.
- The output depends only on what the flattener parsed, so reproducible
  builds no longer depend on bytes nobody reads, such as atime.
- `FORMAT_VERSION` goes to 4 (`.v4.erofs`). Most images build the same
  filesystem as before, but not all: mode type bits, malformed capabilities
  and fields mkfs.erofs read differently no longer reach it. So `.v3`
  artefacts are rebuilt rather than reused.
