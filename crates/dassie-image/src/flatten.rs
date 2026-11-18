//! Flattens an image's layers into one tar stream (ADR 0007).
//!
//! Two passes over the layers, bottom to top:
//!
//! 1. [`Tree::scan`] reads headers only and works out which entry wins at each
//!    path, applying whiteouts the way overlayfs does: a whiteout removes
//!    what lower layers put there, never what its own layer adds.
//! 2. [`Tree::emit`] writes the root (`./`, `0755 root`), then every winning
//!    directory (sorted, so parents precede children), then re-reads the
//!    layers and copies each winning non-directory in layer order, which
//!    keeps hard link targets ahead of their links. `/.dassie/image.json` goes
//!    last.
//!
//! Every output header is written fresh (ADR 0017): one GNU header per entry
//! carrying only what dassie read and checked (type, path, link target,
//! mode, numeric uid/gid, mtime, size, device numbers), plus the entry's
//! `SCHILY.xattr.*` records in namespaces erofs stores. Nothing else in an
//! input header reaches mkfs.erofs, so it cannot read a field differently
//! from the `tar` crate here. Nothing is ever unpacked to a host directory.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};

use tar::{Archive, Builder, EntryType, Header};

/// Directory holding dassie's own files in the rootfs. Image content there is
/// dropped.
pub(crate) const DASSIE_DIR: &str = ".dassie";
const WHITEOUT: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";

/// The root's mode. Images' own root entries are ignored: every rootfs gets
/// `0755 root`, so no image can make `/` writable by its app user.
pub(crate) const ROOT_MODE: u32 = 0o755;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A path or link target with a `..` component or a NUL byte. tar
    /// carries NULs in PAX and GNU long names, but mkfs.erofs, like other C
    /// readers, cuts the name at the first one, so it would build a
    /// different tree from the one checked here.
    BadPath(PathBuf),
    /// An entry type dassie does not carry over: anything but regular
    /// files, directories, symlinks, hard links, devices and FIFOs. That
    /// covers GNU sparse files, and PAX or GNU extension headers in a format
    /// tar did not recognise.
    Unsupported(PathBuf, EntryType),
    /// A header field or PAX record that does not parse, or does not fit
    /// the filesystem (uid and gid are 32-bit in erofs).
    BadField {
        path: PathBuf,
        field: &'static str,
    },
    /// A hard link to a path no lower entry created.
    DanglingLink {
        path: PathBuf,
        target: PathBuf,
    },
    /// A hard link whose target an upper layer replaced or removed.
    LinkTargetChanged {
        path: PathBuf,
        target: PathBuf,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::BadPath(p) => write!(
                f,
                "layer path {} has a `..` component or a NUL byte",
                p.as_os_str().as_encoded_bytes().escape_ascii()
            ),
            Self::Unsupported(p, t) => write!(f, "{}: unsupported entry type {t:?}", p.display()),
            Self::BadField { path, field } => write!(f, "{}: bad {field}", path.display()),
            Self::DanglingLink { path, target } => {
                write!(
                    f,
                    "hard link {} -> missing {}",
                    path.display(),
                    target.display()
                )
            }
            Self::LinkTargetChanged { path, target } => write!(
                f,
                "hard link {} -> {}, which a later layer replaced",
                path.display(),
                target.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Where a winning entry comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The `index`-th entry of layer `layer`.
    Entry { layer: usize, index: usize },
    /// A parent directory the layer `layer` implied but did not contain.
    Implied { layer: usize },
}

#[derive(Debug, Clone)]
enum Kind {
    /// Directories are emitted from what pass 1 read.
    Dir(Meta),
    Hardlink {
        target: PathBuf,
        target_source: Source,
    },
    /// Files, symlinks and special files: written from the layer in pass 2.
    Other,
}

#[derive(Debug, Clone)]
struct Node {
    source: Source,
    kind: Kind,
}

/// The winning entry at every path, after all layers are applied.
#[derive(Debug, Default)]
pub struct Tree {
    nodes: BTreeMap<PathBuf, Node>,
    layers: usize,
}

/// One of a layer's own entries, deferred until its markers are applied.
enum Step {
    /// Make every ancestor of the path a directory.
    Parents(PathBuf),
    Put {
        path: PathBuf,
        index: usize,
        kind: Kind,
    },
    /// Resolved when put, against the tree as it is at that point.
    Link {
        path: PathBuf,
        index: usize,
        target: PathBuf,
    },
}

/// What one tar entry does to the tree.
enum Action {
    Skip,
    /// `.wh.<name>`: remove `path` from lower layers.
    Whiteout(PathBuf),
    /// `.wh..wh..opq`: remove the children of `path` from lower layers.
    Opaque(PathBuf),
    Put(PathBuf),
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pass 1: applies the next layer (an uncompressed tar stream). Reads it
    /// to EOF, so a digest-verifying reader underneath gets to check.
    ///
    /// A whiteout or opaque marker hides only what lower layers put there,
    /// wherever it sits in this layer's tar. So the layer's headers are read
    /// first, its markers applied to the tree of lower layers, and only then
    /// its own entries put in tar order.
    pub fn scan(&mut self, layer: impl Read) -> Result<(), Error> {
        let n = self.layers;
        self.layers += 1;
        let mut archive = Archive::new(layer);
        let mut removals = Vec::new();
        let mut steps = Vec::new();
        for (index, entry) in archive.entries()?.enumerate() {
            let mut entry = entry?;
            match classify(&entry)? {
                Action::Skip => {}
                Action::Whiteout(path) => {
                    // The marker lives in its parent, so the layer has that
                    // directory, as overlayfs, Docker and containerd agree.
                    steps.push(Step::Parents(path.clone()));
                    removals.push((path, true));
                }
                Action::Opaque(dir) => {
                    // The directory itself stays; make sure it exists.
                    steps.push(Step::Parents(dir.join(OPAQUE)));
                    removals.push((dir, false));
                }
                Action::Put(path) => steps.push(match entry.header().entry_type() {
                    EntryType::Directory => Step::Put {
                        kind: Kind::Dir(Meta::read(&path, &mut entry)?),
                        path,
                        index,
                    },
                    EntryType::Link => Step::Link {
                        path,
                        index,
                        target: link_target(&entry)?,
                    },
                    _ => {
                        // Checked now, so a bad field fails the scan rather
                        // than halfway through writing the output.
                        Meta::read(&path, &mut entry)?;
                        Step::Put {
                            path,
                            index,
                            kind: Kind::Other,
                        }
                    }
                }),
            }
        }
        io::copy(&mut archive.into_inner(), &mut io::sink())?;

        for (path, itself) in removals {
            self.remove_children(&path);
            if itself {
                self.nodes.remove(&path);
            }
        }
        for step in steps {
            let (path, index, kind) = match step {
                Step::Parents(path) => {
                    self.parents(&path, n);
                    continue;
                }
                Step::Put { path, index, kind } => (path, index, kind),
                Step::Link {
                    path,
                    index,
                    target,
                } => {
                    let target_source = self
                        .nodes
                        .get(&target)
                        .filter(|t| !matches!(t.kind, Kind::Dir(_)))
                        .map(|t| t.source)
                        .ok_or_else(|| Error::DanglingLink {
                            path: path.clone(),
                            target: target.clone(),
                        })?;
                    let kind = Kind::Hardlink {
                        target,
                        target_source,
                    };
                    (path, index, kind)
                }
            };
            let source = Source::Entry { layer: n, index };
            self.put(path, Node { source, kind }, n);
        }
        Ok(())
    }

    /// Checks hard links still point at what they linked to.
    pub fn finish(&self) -> Result<(), Error> {
        for (path, node) in &self.nodes {
            if let Kind::Hardlink {
                target,
                target_source,
            } = &node.kind
                && self.nodes.get(target).map(|t| t.source) != Some(*target_source)
            {
                return Err(Error::LinkTargetChanged {
                    path: path.clone(),
                    target: target.clone(),
                });
            }
        }
        Ok(())
    }

    /// Pass 2: writes the flattened tar to `out`. `open(i)` must return layer
    /// `i` exactly as [`Self::scan`] saw it.
    pub fn emit<R: Read, W: Write>(
        &self,
        mut open: impl FnMut(usize) -> Result<R, Error>,
        image_config: &[u8],
        out: W,
    ) -> Result<W, Error> {
        let mut tar = Builder::new(out);
        // mkfs.erofs makes a root with no entry `0777`.
        let mut root = owned_header(EntryType::Directory, ROOT_MODE, 0);
        root.set_path("./")?;
        root.set_cksum();
        tar.append(&root, io::empty())?;
        for (path, node) in &self.nodes {
            if let Kind::Dir(meta) = &node.kind {
                let mut header = meta.header(EntryType::Directory, 0);
                append_xattrs(&mut tar, meta)?;
                tar.append_data(&mut header, path, io::empty())?;
            }
        }
        for layer in 0..self.layers {
            let mut archive = Archive::new(open(layer)?);
            for (index, entry) in archive.entries()?.enumerate() {
                let mut entry = entry?;
                let Action::Put(path) = classify(&entry)? else {
                    continue;
                };
                let Some(node) = self.nodes.get(&path) else {
                    continue;
                };
                if node.source != (Source::Entry { layer, index }) {
                    continue;
                }
                match &node.kind {
                    Kind::Dir(_) => {}
                    // A hard link names its target's inode: mkfs.erofs
                    // takes nothing else from its header.
                    Kind::Hardlink { target, .. } => {
                        let mut header = Meta::owned(0o644).header(EntryType::Link, 0);
                        tar.append_link(&mut header, &path, target)?;
                    }
                    Kind::Other => {
                        let meta = Meta::read(&path, &mut entry)?;
                        append_xattrs(&mut tar, &meta)?;
                        match entry.header().entry_type() {
                            t @ EntryType::Symlink => {
                                let target = entry.link_name_bytes().unwrap_or_default();
                                let mut header = meta.header(t, 0);
                                append_symlink(&mut tar, &mut header, &path, &target)?;
                            }
                            t @ (EntryType::Char | EntryType::Block) => {
                                let mut header = meta.header(t, 0);
                                let (major, minor) = device(&path, entry.header())?;
                                header.set_device_major(major)?;
                                header.set_device_minor(minor)?;
                                tar.append_data(&mut header, &path, io::empty())?;
                            }
                            t @ EntryType::Fifo => {
                                let mut header = meta.header(t, 0);
                                tar.append_data(&mut header, &path, io::empty())?;
                            }
                            // Regular or contiguous (`classify` allows no
                            // other type here): both are plain files.
                            _ => {
                                let mut header = meta.header(EntryType::Regular, entry.size());
                                tar.append_data(&mut header, &path, &mut entry)?;
                            }
                        }
                    }
                }
            }
            // Read to EOF so a verifying reader checks this pass too: the blob
            // could have changed since `scan`.
            io::copy(&mut archive.into_inner(), &mut io::sink())?;
        }
        let mut dir = owned_header(EntryType::Directory, 0o755, 0);
        tar.append_data(&mut dir, DASSIE_DIR, io::empty())?;
        let mut file = owned_header(EntryType::Regular, 0o644, image_config.len() as u64);
        tar.append_data(
            &mut file,
            Path::new(DASSIE_DIR).join("image.json"),
            image_config,
        )?;
        Ok(tar.into_inner()?)
    }

    /// Inserts a node, first making sure every ancestor is a directory.
    fn put(&mut self, path: PathBuf, node: Node, layer: usize) {
        self.parents(&path, layer);
        let replaces_dir = self
            .nodes
            .get(&path)
            .is_some_and(|old| matches!(old.kind, Kind::Dir(_)));
        if replaces_dir && !matches!(node.kind, Kind::Dir(_)) {
            self.remove_children(&path);
        }
        self.nodes.insert(path, node);
    }

    /// Ensures each ancestor of `path` is a directory, implying any that are
    /// missing (or were not directories, as overlayfs would).
    fn parents(&mut self, path: &Path, layer: usize) {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            let is_dir = self
                .nodes
                .get(ancestor)
                .is_some_and(|n| matches!(n.kind, Kind::Dir(_)));
            if !is_dir {
                self.nodes.insert(
                    ancestor.to_path_buf(),
                    Node {
                        source: Source::Implied { layer },
                        kind: Kind::Dir(Meta::owned(0o755)),
                    },
                );
            }
        }
    }

    /// Removes the descendants of `path`.
    fn remove_children(&mut self, path: &Path) {
        // `PathBuf` orders by component, so a path's descendants follow it
        // contiguously.
        let doomed: Vec<PathBuf> = self
            .nodes
            .range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
            .take_while(|(p, _)| p.starts_with(path))
            .map(|(p, _)| p.clone())
            .collect();
        for p in doomed {
            self.nodes.remove(&p);
        }
    }
}

/// Decides what an entry does, with its path normalised.
fn classify<R: Read>(entry: &tar::Entry<'_, R>) -> Result<Action, Error> {
    let entry_type = entry.header().entry_type();
    if entry_type.is_pax_global_extensions() {
        return Ok(Action::Skip);
    }
    let Some(path) = normalise(&entry.path()?)? else {
        // The root itself: `emit` writes its own.
        return Ok(Action::Skip);
    };
    if path.starts_with(DASSIE_DIR) {
        return Ok(Action::Skip);
    }
    // Anything else has no inode to carry over, or (GNU sparse files) data
    // tar does not decode. tar also hands over PAX and GNU long name headers
    // as entries when it does not recognise the header's format, so it has
    // not applied them.
    if !matches!(
        entry_type,
        EntryType::Regular
            | EntryType::Continuous
            | EntryType::Directory
            | EntryType::Symlink
            | EntryType::Link
            | EntryType::Char
            | EntryType::Block
            | EntryType::Fifo
    ) {
        return Err(Error::Unsupported(path, entry_type));
    }
    // Symlink targets are not paths in the tree, so `normalise` never sees
    // them, but mkfs.erofs would cut them at a NUL too.
    if entry_type == EntryType::Symlink
        && let Some(target) = entry.link_name()?
        && has_nul(target.as_os_str())
    {
        return Err(Error::BadPath(target.into_owned()));
    }
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    let parent = path.parent().map_or_default(Path::to_path_buf);
    Ok(if name == OPAQUE {
        Action::Opaque(parent)
    } else if name.starts_with(".wh..wh.") {
        // Other aufs metadata (`.wh..wh.plnk`, …).
        Action::Skip
    } else if let Some(hidden) = name.strip_prefix(WHITEOUT) {
        Action::Whiteout(parent.join(hidden))
    } else {
        Action::Put(path)
    })
}

/// Strips `/` and `.`, rejects `..` and NUL bytes. `None` means the root.
fn normalise(path: &Path) -> Result<Option<PathBuf>, Error> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(part) if !has_nul(part) => out.push(part),
            Component::Normal(_) => return Err(Error::BadPath(path.to_path_buf())),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(Error::BadPath(path.to_path_buf()));
            }
        }
    }
    Ok((!out.as_os_str().is_empty()).then_some(out))
}

fn has_nul(s: &OsStr) -> bool {
    s.as_encoded_bytes().contains(&0)
}

fn link_target<R: Read>(entry: &tar::Entry<'_, R>) -> Result<PathBuf, Error> {
    let raw = entry.link_name()?.unwrap_or_default();
    normalise(&raw)?.ok_or_else(|| Error::BadPath(raw.into_owned()))
}

const XATTR_PREFIX: &str = "SCHILY.xattr.";

/// xattr namespaces erofs stores, besides the two POSIX ACLs. mkfs.erofs
/// drops others (macOS's `com.apple.*`, for one), so they are dropped here
/// too.
const XATTR_NAMESPACES: [&str; 3] = ["user.", "trusted.", "security."];

/// What an entry's inode carries into the rootfs. Nothing else in its header
/// or PAX records is written out.
#[derive(Debug, Clone)]
struct Meta {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    /// `SCHILY.xattr.*` records in [`XATTR_NAMESPACES`], keys as written.
    xattrs: Vec<(String, Vec<u8>)>,
}

impl Meta {
    fn owned(mode: u32) -> Self {
        Self {
            mode,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: Vec::new(),
        }
    }

    /// Reads the entry's header fields, with PAX `uid`, `gid` and `mtime`
    /// records in their place where present, as POSIX says.
    fn read<R: Read>(path: &Path, entry: &mut tar::Entry<'_, R>) -> Result<Self, Error> {
        let bad = |field| Error::BadField {
            path: path.to_path_buf(),
            field,
        };
        let header = entry.header();
        let old = header.as_old();
        // GNU tar and mkfs.erofs read an empty field as 0; tar refuses it.
        let number = |value: io::Result<u64>, raw: &[u8], field| match value {
            Ok(v) => Ok(v),
            Err(_) if raw.iter().all(|&b| b == 0 || b == b' ') => Ok(0),
            Err(_) => Err(bad(field)),
        };
        // The masked value always fits.
        let mode = (number(header.mode().map(u64::from), &old.mode, "mode")? & 0o7777) as u32;
        let mut uid = number(header.uid(), &old.uid, "uid")?;
        let mut gid = number(header.gid(), &old.gid, "gid")?;
        let mut mtime = number(header.mtime(), &old.mtime, "mtime")?;
        // The last record for a key wins, as in mkfs.erofs, before any is
        // judged.
        let mut records = BTreeMap::new();
        if let Some(exts) = entry.pax_extensions()? {
            for ext in exts {
                let ext = ext?;
                let key = ext.key().map_err(|_| bad("PAX key"))?;
                let text = || std::str::from_utf8(ext.value_bytes()).ok();
                match key {
                    "uid" => {
                        uid = text()
                            .and_then(|v| v.parse().ok())
                            .ok_or_else(|| bad("uid"))?;
                    }
                    "gid" => {
                        gid = text()
                            .and_then(|v| v.parse().ok())
                            .ok_or_else(|| bad("gid"))?;
                    }
                    "mtime" => {
                        mtime = text().and_then(pax_time).ok_or_else(|| bad("mtime"))?;
                    }
                    _ if key.starts_with(XATTR_PREFIX) => {
                        records.insert(key.to_owned(), ext.value_bytes().to_vec());
                    }
                    _ => {}
                }
            }
        }
        let xattrs = records
            .into_iter()
            .filter(|(k, v)| {
                k.strip_prefix(XATTR_PREFIX)
                    .is_some_and(|n| keep_xattr(n, v))
            })
            .collect();
        Ok(Self {
            mode,
            uid: uid.try_into().map_err(|_| bad("uid"))?,
            gid: gid.try_into().map_err(|_| bad("gid"))?,
            mtime,
            xattrs,
        })
    }

    /// A fresh GNU header: long names go in `././@LongLink` entries, never a
    /// ustar prefix, and numbers too big for octal in base-256.
    fn header(&self, entry_type: EntryType, size: u64) -> Header {
        let mut h = Header::new_gnu();
        h.set_entry_type(entry_type);
        h.set_mode(self.mode);
        h.set_uid(self.uid.into());
        h.set_gid(self.gid.into());
        h.set_mtime(self.mtime);
        h.set_size(size);
        h
    }
}

/// Whether an xattr reaches the guest as written: a name in a namespace
/// erofs stores, and a value the kernel will read back for the two it
/// parses. It refuses a malformed one, so the file has no capabilities or
/// ACL either way.
fn keep_xattr(name: &str, value: &[u8]) -> bool {
    if name.contains('\0') {
        return false;
    }
    match name {
        "security.capability" => valid_capability(value),
        "system.posix_acl_access" | "system.posix_acl_default" => valid_acl(value),
        _ => XATTR_NAMESPACES
            .iter()
            .any(|ns| name.len() > ns.len() && name.starts_with(ns)),
    }
}

/// A POSIX ACL xattr with at least one entry: version 2, then 8-byte
/// `(tag, perm, id)` entries. The kernel reads a header with no entries as
/// no ACL, and refuses other versions and sizes. It does not check the
/// entries themselves on read.
fn valid_acl(value: &[u8]) -> bool {
    value.first_chunk::<4>().map(|v| u32::from_le_bytes(*v)) == Some(2)
        && value.len() > 4
        && (value.len() - 4).is_multiple_of(8)
}

/// A `vfs_cap_data` of revision 1, 2 or 3, the size its revision says.
fn valid_capability(value: &[u8]) -> bool {
    let Some(magic) = value.first_chunk::<4>() else {
        return false;
    };
    let size = match u32::from_le_bytes(*magic) & 0xff00_0000 {
        0x0100_0000 => 12,
        0x0200_0000 => 20,
        0x0300_0000 => 24,
        _ => return false,
    };
    value.len() == size
}

/// A PAX time (`123`, `123.456`, `-1.5`) in whole seconds. Times before the
/// epoch become 0: erofs stores them unsigned.
fn pax_time(s: &str) -> Option<u64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, ""));
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: i64 = secs.parse().ok()?;
    Some(secs.try_into().unwrap_or(0))
}

fn device(path: &Path, header: &Header) -> Result<(u32, u32), Error> {
    let bad = |field| Error::BadField {
        path: path.to_path_buf(),
        field,
    };
    let major = header
        .device_major()
        .ok()
        .flatten()
        .ok_or_else(|| bad("device major"))?;
    let minor = header
        .device_minor()
        .ok()
        .flatten()
        .ok_or_else(|| bad("device minor"))?;
    Ok((major, minor))
}

/// Writes a symlink with its target byte for byte. `Builder::append_link`
/// drops `.` components and trailing slashes from a target that fits the
/// header (or would, cleaned up), which changes what `readlink` returns. A
/// longer target goes in a `././@LongLink` entry of type `K`, as the builder
/// would write it.
fn append_symlink<W: Write>(
    tar: &mut Builder<W>,
    header: &mut Header,
    path: &Path,
    target: &[u8],
) -> io::Result<()> {
    let field = header.as_old().linkname.len();
    if target.len() > field {
        let mut long = owned_header(EntryType::GNULongLink, 0o644, target.len() as u64 + 1);
        long.as_gnu_mut()
            .ok_or_else(|| io::Error::other("not a GNU header"))?
            .name[..13]
            .copy_from_slice(b"././@LongLink");
        long.set_cksum();
        tar.append(&long, target.chain(&[0][..]))?;
    }
    header.set_link_name_literal(target.get(..field).unwrap_or(target))?;
    tar.append_data(header, path, io::empty())
}

fn append_xattrs<W: Write>(tar: &mut Builder<W>, meta: &Meta) -> io::Result<()> {
    if meta.xattrs.is_empty() {
        return Ok(());
    }
    tar.append_pax_extensions(meta.xattrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))
}

/// A root-owned header with a fixed mtime, so builds are reproducible.
pub(crate) fn owned_header(entry_type: EntryType, mode: u32, size: u64) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(entry_type);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_size(size);
    h
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A revision 2 `vfs_cap_data` granting `CAP_NET_RAW`, as setcap writes it.
    const CAP: [u8; 20] = [
        0x01, 0, 0, 0x02, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// Builds layer tars in memory.
    pub(crate) struct Layer(Builder<Vec<u8>>);

    impl Default for Layer {
        fn default() -> Self {
            Self(Builder::new(Vec::new()))
        }
    }

    impl Layer {
        fn header(t: EntryType, mode: u32, uid: u64, gid: u64, size: u64) -> Header {
            let mut h = Header::new_ustar();
            h.set_entry_type(t);
            h.set_mode(mode);
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_mtime(1_700_000_000);
            h.set_size(size);
            h
        }

        pub(crate) fn dir(mut self, path: &str, mode: u32, uid: u64) -> Self {
            let mut h = Self::header(EntryType::Directory, mode, uid, uid, 0);
            self.0.append_data(&mut h, path, io::empty()).unwrap();
            self
        }

        pub(crate) fn file(mut self, path: &str, data: &str, mode: u32, uid: u64) -> Self {
            let mut h = Self::header(EntryType::Regular, mode, uid, uid, data.len() as u64);
            self.0.append_data(&mut h, path, data.as_bytes()).unwrap();
            self
        }

        fn link(mut self, t: EntryType, path: &str, target: &str) -> Self {
            let mut h = Self::header(t, 0o777, 0, 0, 0);
            self.0.append_link(&mut h, path, target).unwrap();
            self
        }

        pub(crate) fn symlink(self, path: &str, target: &str) -> Self {
            self.link(EntryType::Symlink, path, target)
        }

        fn hardlink(self, path: &str, target: &str) -> Self {
            self.link(EntryType::Link, path, target)
        }

        fn char_dev(mut self, path: &str, major: u32, minor: u32) -> Self {
            let mut h = Self::header(EntryType::Char, 0o666, 0, 0, 0);
            h.set_device_major(major).unwrap();
            h.set_device_minor(minor).unwrap();
            self.0.append_data(&mut h, path, io::empty()).unwrap();
            self
        }

        /// Sets an xattr on the entry added next (PAX headers precede it).
        fn xattr(mut self, name: &str, value: &[u8]) -> Self {
            let key = format!("SCHILY.xattr.{name}");
            self.0
                .append_pax_extensions([(key.as_str(), value)])
                .unwrap();
            self
        }

        pub(crate) fn tar(self) -> Vec<u8> {
            self.0.into_inner().unwrap()
        }
    }

    /// One entry of the flattened output.
    #[derive(Debug, PartialEq, Eq)]
    struct Out {
        path: String,
        kind: EntryType,
        mode: u32,
        uid: u64,
        gid: u64,
        mtime: u64,
        data: String,
        link: String,
        xattrs: Vec<(String, Vec<u8>)>,
    }

    /// The flattened output after its root entry, which every output starts
    /// with (see [`root_comes_first_and_ignores_the_images`]).
    fn flatten(layers: &[Vec<u8>], config: &[u8]) -> Result<Vec<Out>, Error> {
        let mut out = flatten_all(layers, config)?;
        assert_eq!(out.remove(0).path, "./");
        Ok(out)
    }

    fn flatten_all(layers: &[Vec<u8>], config: &[u8]) -> Result<Vec<Out>, Error> {
        let mut tree = Tree::new();
        for l in layers {
            tree.scan(l.as_slice())?;
        }
        tree.finish()?;
        let out = tree.emit(|i| Ok(layers[i].as_slice()), config, Vec::new())?;
        let mut archive = Archive::new(out.as_slice());
        Ok(archive
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                // Every PAX record the output carries, unfiltered.
                let xattrs = e.pax_extensions().unwrap().map_or_default(|exts| {
                    exts.map(|x| {
                        let x = x.unwrap();
                        (x.key().unwrap().to_owned(), x.value_bytes().to_vec())
                    })
                    .collect()
                });
                let h = e.header().clone();
                let mut data = String::new();
                e.read_to_string(&mut data).unwrap();
                Out {
                    path: e.path().unwrap().to_string_lossy().into_owned(),
                    kind: h.entry_type(),
                    mode: h.mode().unwrap(),
                    uid: h.uid().unwrap(),
                    gid: h.gid().unwrap(),
                    mtime: h.mtime().unwrap(),
                    data,
                    link: e
                        .link_name()
                        .unwrap()
                        .map_or_default(|l| l.to_string_lossy().into_owned()),
                    xattrs,
                }
            })
            .collect())
    }

    fn paths(out: &[Out]) -> Vec<&str> {
        out.iter().map(|o| o.path.as_str()).collect()
    }

    fn find<'a>(out: &'a [Out], path: &str) -> &'a Out {
        out.iter().find(|o| o.path == path).unwrap()
    }

    #[test]
    fn upper_layers_win_and_whiteouts_remove() {
        let base = Layer::default()
            .dir("etc", 0o755, 0)
            .file("etc/keep", "base", 0o644, 0)
            .file("etc/gone", "x", 0o644, 0)
            .dir("var/cache", 0o755, 0)
            .file("var/cache/a", "a", 0o644, 0)
            .file("var/cache/b", "b", 0o644, 0)
            .tar();
        let upper = Layer::default()
            .file("etc/keep", "upper", 0o600, 0)
            .file("etc/.wh.gone", "", 0o644, 0)
            .dir("var/cache", 0o700, 101)
            .file("var/cache/.wh..wh..opq", "", 0o644, 0)
            .file("var/cache/c", "c", 0o644, 101)
            .tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(
            paths(&out),
            [
                "etc",
                "var",
                "var/cache",
                "etc/keep",
                "var/cache/c",
                ".dassie",
                ".dassie/image.json"
            ]
        );
        assert_eq!(find(&out, "etc/keep").data, "upper");
        assert_eq!(find(&out, "etc/keep").mode, 0o600);
        // The upper layer's directory metadata wins.
        let cache = find(&out, "var/cache");
        assert_eq!((cache.mode, cache.uid), (0o700, 101));
    }

    #[test]
    fn whiteout_does_not_remove_its_own_layer() {
        // Not what tools emit, but overlayfs semantics say a whiteout only
        // hides lower layers.
        let base = Layer::default().file("a", "base", 0o644, 0).tar();
        let upper = Layer::default()
            .file("a", "upper", 0o644, 0)
            .file(".wh.a", "", 0o644, 0)
            .tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(find(&out, "a").data, "upper");
    }

    #[test]
    fn whiteout_after_own_children_keeps_their_parent() {
        let base = Layer::default()
            .dir("d", 0o700, 1000)
            .file("d/old", "x", 0o644, 0)
            .tar();
        let upper = Layer::default()
            .file("d/new", "y", 0o644, 0)
            .file(".wh.d", "", 0o644, 0)
            .tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(paths(&out), ["d", "d/new", ".dassie", ".dassie/image.json"]);
        // The lower `d` is gone; this one is implied by the upper layer.
        let d = find(&out, "d");
        assert_eq!((d.mode, d.uid), (0o755, 0));
    }

    #[test]
    fn whiteout_implies_its_parent() {
        // Found by the flatten_model fuzz target: a whiteout in a directory
        // no layer created, or in one a lower layer made a file.
        let base = Layer::default().file("f", "x", 0o644, 0).tar();
        let upper = Layer::default()
            .file("d/.wh.x", "", 0o644, 0)
            .file("f/.wh.x", "", 0o644, 0)
            .tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(paths(&out), ["d", "f", ".dassie", ".dassie/image.json"]);
        assert_eq!(find(&out, "f").kind, EntryType::Directory);
    }

    #[test]
    fn late_whiteout_keeps_a_markers_parent() {
        // Found by the flatten_model fuzz target: the upper layer has `d`
        // (it holds a marker), so the later whiteout of the lower `d` must
        // not take it away.
        let base = Layer::default()
            .file("d/old", "x", 0o644, 0)
            .file("o/old", "x", 0o644, 0)
            .tar();
        let upper = Layer::default()
            .file("d/.wh.x", "", 0o644, 0)
            .file(".wh.d", "", 0o644, 0)
            .file("o/.wh..wh..opq", "", 0o644, 0)
            .file(".wh..wh..opq", "", 0o644, 0)
            .tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(paths(&out), ["d", "o", ".dassie", ".dassie/image.json"]);
    }

    #[test]
    fn rejects_extension_headers_tar_did_not_apply() {
        // Found by the flatten_bytes fuzz target: in a v7 header, tar yields
        // a PAX header as an entry of its own. Copied out, it corrupted the
        // stream.
        for ty in [EntryType::XHeader, EntryType::GNULongName] {
            let mut tar = Builder::new(Vec::new());
            let mut h = Header::new_old();
            h.set_path("ext").unwrap();
            h.set_entry_type(ty);
            h.set_size(16);
            h.set_cksum();
            tar.append(&h, b"16 path=.dassie\n".as_slice()).unwrap();
            let layer = Layer(tar).file("a", "x", 0o644, 0).tar();
            let err = flatten(&[layer], b"{}").unwrap_err();
            assert!(matches!(err, Error::Unsupported(_, t) if t == ty), "{err}");
        }
    }

    #[test]
    fn late_whiteouts_keep_every_survivors_parents() {
        let base = Layer::default()
            .dir("d/a", 0o755, 0)
            .dir("d/b", 0o755, 0)
            .dir("o/a", 0o755, 0)
            .file("o/a/old", "x", 0o644, 0)
            .tar();
        let upper = Layer::default()
            .file("d/a/x", "x", 0o644, 0)
            .file("d/b/y", "y", 0o644, 0)
            .file(".wh.d", "", 0o644, 0)
            .file("o/a/new", "n", 0o644, 0)
            .file("o/.wh..wh..opq", "", 0o644, 0)
            .tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(
            paths(&out),
            [
                "d",
                "d/a",
                "d/b",
                "o",
                "o/a",
                "d/a/x",
                "d/b/y",
                "o/a/new",
                ".dassie",
                ".dassie/image.json"
            ]
        );
    }

    #[test]
    fn emit_verifies_layers_again() {
        let layer = Layer::default().file("a", "x", 0o644, 0).tar();
        let mut tree = Tree::new();
        tree.scan(layer.as_slice()).unwrap();
        // The second read fails at EOF, like a blob changed between passes.
        let changed = crate::VerifyingReader::new(layer.as_slice(), crate::Digest::of(b"other"));
        let mut opened = Some(changed);
        let err = tree
            .emit(|_| Ok(opened.take().unwrap()), b"{}", Vec::new())
            .unwrap_err();
        assert!(matches!(err, Error::Io(e) if e.kind() == io::ErrorKind::InvalidData));
    }

    #[test]
    fn file_replacing_dir_drops_its_children() {
        let base = Layer::default()
            .dir("d", 0o755, 0)
            .file("d/f", "x", 0o644, 0)
            .tar();
        let upper = Layer::default().file("d", "now a file", 0o644, 0).tar();
        let out = flatten(&[base, upper], b"{}").unwrap();
        assert_eq!(paths(&out), ["d", ".dassie", ".dassie/image.json"]);
        assert_eq!(find(&out, "d").kind, EntryType::Regular);
    }

    #[test]
    fn parents_are_implied_and_emitted_first() {
        let layer = Layer::default()
            .file("usr/local/bin/app", "#!", 0o755, 0)
            .tar();
        let out = flatten(&[layer], b"{}").unwrap();
        assert_eq!(
            paths(&out)[..4],
            ["usr", "usr/local", "usr/local/bin", "usr/local/bin/app"]
        );
        let usr = find(&out, "usr");
        assert_eq!(
            (usr.kind, usr.mode, usr.uid),
            (EntryType::Directory, 0o755, 0)
        );
    }

    #[test]
    fn ownership_modes_devices_and_xattrs_survive() {
        let layer = Layer::default()
            .dir("home/app", 0o700, 1000)
            .file("srv/data", "d", 0o640, 101)
            .file("usr/bin/su", "elf", 0o4755, 0)
            .xattr("security.capability", &CAP)
            .file("usr/bin/ping", "elf", 0o755, 0)
            .char_dev("dev/null", 1, 3)
            .symlink("lib", "usr/lib")
            .tar();
        let out = flatten(&[layer], b"{}").unwrap();
        let home = find(&out, "home/app");
        assert_eq!((home.mode, home.uid, home.gid), (0o700, 1000, 1000));
        let data = find(&out, "srv/data");
        assert_eq!((data.mode, data.uid, data.gid), (0o640, 101, 101));
        assert_eq!(find(&out, "usr/bin/su").mode, 0o4755);
        assert_eq!(
            find(&out, "usr/bin/ping").xattrs,
            [("SCHILY.xattr.security.capability".to_owned(), CAP.to_vec())]
        );
        assert_eq!(find(&out, "dev/null").kind, EntryType::Char);
        assert_eq!(find(&out, "lib").link, "usr/lib");
    }

    #[test]
    fn hard_links_across_layers() {
        let base = Layer::default().file("bin/busybox", "elf", 0o755, 0).tar();
        let upper = Layer::default().hardlink("bin/sh", "bin/busybox").tar();
        let out = flatten(&[base.clone(), upper], b"{}").unwrap();
        let p = paths(&out);
        let target = p.iter().position(|&x| x == "bin/busybox").unwrap();
        let link = p.iter().position(|&x| x == "bin/sh").unwrap();
        assert!(target < link);
        assert_eq!(find(&out, "bin/sh").link, "bin/busybox");

        let linked = Layer::default().hardlink("bin/sh", "bin/busybox").tar();
        let replaced = Layer::default().file("bin/busybox", "new", 0o755, 0).tar();
        assert!(matches!(
            flatten(&[base, linked, replaced], b"{}"),
            Err(Error::LinkTargetChanged { .. })
        ));
        let dangling = Layer::default().hardlink("bin/sh", "bin/nope").tar();
        assert!(matches!(
            flatten(&[dangling], b"{}"),
            Err(Error::DanglingLink { .. })
        ));
    }

    #[test]
    fn image_config_is_appended_and_image_dassie_dropped() {
        let layer = Layer::default()
            .file(".dassie/image.json", "evil", 0o644, 0)
            .tar();
        let config = br#"{"config":{"Cmd":["/app"]}}"#;
        let out = flatten(&[layer], config).unwrap();
        assert_eq!(paths(&out), [".dassie", ".dassie/image.json"]);
        assert_eq!(find(&out, ".dassie/image.json").data.as_bytes(), config);
    }

    #[test]
    fn parent_components_are_rejected() {
        // `tar::Builder` refuses to write `..`, so patch the name in place.
        let mut layer = Layer::default().file("aa/passwd", "x", 0o644, 0).tar();
        layer[..9].copy_from_slice(b"../passwd");
        let mut header = Header::new_old();
        header.as_mut_bytes().copy_from_slice(&layer[..512]);
        header.set_cksum();
        layer[..512].copy_from_slice(header.as_bytes());
        assert!(matches!(flatten(&[layer], b"{}"), Err(Error::BadPath(_))));
    }

    #[test]
    fn nul_bytes_in_paths_and_link_targets_are_rejected() {
        // PAX records carry NULs, and tar reads them back, but mkfs.erofs
        // cuts names at the first one: `<name>\0x` would land at `<name>`.
        // Past 100 bytes the builder writes them out in GNU long names.
        let name = "a".repeat(120);
        let bad = format!("{name}\0x");
        let base = Layer::default().file(&name, "x", 0o644, 0).tar();
        for (key, entry) in [
            ("path", EntryType::Regular),
            ("linkpath", EntryType::Link),
            ("linkpath", EntryType::Symlink),
        ] {
            let mut tar = Builder::new(Vec::new());
            tar.append_pax_extensions([(key, bad.as_bytes())]).unwrap();
            // The PAX record overrides the placeholder names here.
            let layer = match entry {
                EntryType::Regular => Layer(tar).file("b", "y", 0o644, 0),
                EntryType::Link => Layer(tar).hardlink("b", "a"),
                _ => Layer(tar).symlink("b", "a"),
            }
            .tar();
            let err = flatten(&[base.clone(), layer], b"{}").unwrap_err();
            assert!(
                matches!(&err, Error::BadPath(p) if p.as_os_str() == bad.as_str()),
                "{key} on {entry:?}: {err}"
            );
        }
    }

    #[test]
    fn root_comes_first_and_ignores_the_images() {
        let layer = Layer::default()
            .dir("./", 0o777, 1000)
            .file("a", "x", 0o644, 0)
            .tar();
        let out = flatten_all(&[layer], b"{}").unwrap();
        assert_eq!(paths(&out), ["./", "a", ".dassie", ".dassie/image.json"]);
        let root = &out[0];
        assert_eq!(
            (root.kind, root.mode, root.uid, root.gid),
            (EntryType::Directory, 0o755, 0, 0)
        );
    }

    #[test]
    fn gnu_access_and_change_times_are_cleared() {
        let mut tar = Builder::new(Vec::new());
        for (t, path, data) in [
            (EntryType::Directory, "bin", &b""[..]),
            (EntryType::Regular, "bin/app", &b"x"[..]),
        ] {
            let mut h = Header::new_gnu();
            h.set_entry_type(t);
            h.set_mode(0o755);
            h.set_size(data.len() as u64);
            let gnu = h.as_gnu_mut().unwrap();
            gnu.atime = *b"00000000001\0";
            gnu.ctime = *b"00000000002\0";
            tar.append_data(&mut h, path, data).unwrap();
        }
        let layer = tar.into_inner().unwrap();

        let mut tree = Tree::new();
        tree.scan(layer.as_slice()).unwrap();
        tree.finish().unwrap();
        let out = tree
            .emit(|_| Ok(layer.as_slice()), b"{}", Vec::new())
            .unwrap();
        let mut archive = Archive::new(out.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            if let Some(gnu) = entry.header().as_gnu() {
                assert_eq!((gnu.atime, gnu.ctime), ([0; 12], [0; 12]));
            }
            seen.push(entry.path().unwrap().to_string_lossy().into_owned());
        }
        assert_eq!(seen[1..3], ["bin", "bin/app"]);
    }

    #[test]
    fn ustar_prefixes_are_not_applied_twice() {
        // Found by the flatten_bytes fuzz target: `d/f` held as prefix `d`
        // and name `f` came out as `d/d/f`, ahead of any `d/d`, because
        // `Header::set_path` leaves the prefix alone when the path fits the
        // name field.
        let mut tar = Builder::new(Vec::new());
        for (t, prefix, name, data) in [
            (EntryType::Directory, b"d", "e", &b""[..]),
            (EntryType::Regular, b"d", "f", &b"x"[..]),
        ] {
            let mut h = Layer::header(t, 0o755, 0, 0, data.len() as u64);
            h.set_path(name).unwrap();
            h.as_ustar_mut().unwrap().prefix[..1].copy_from_slice(prefix);
            h.set_cksum();
            tar.append(&h, data).unwrap();
        }
        let layer = tar.into_inner().unwrap();
        let out = flatten(&[layer], b"{}").unwrap();
        assert_eq!(
            paths(&out),
            ["d", "d/e", "d/f", ".dassie", ".dassie/image.json"]
        );
    }

    /// A layer whose one entry, `f`, has `ty` and the PAX records `pax`.
    fn with_pax(ty: EntryType, pax: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = Builder::new(Vec::new());
        tar.append_pax_extensions(pax.iter().copied()).unwrap();
        let mut h = Layer::header(ty, 0o644, 1, 1, 1);
        tar.append_data(&mut h, "f", b"x".as_slice()).unwrap();
        tar.into_inner().unwrap()
    }

    #[test]
    fn output_headers_are_written_fresh() {
        // Found by the flatten_erofs fuzz target: copied v7 headers and
        // headers with a mangled magic made mkfs.erofs fail ("invalid tar
        // magic", "chksum mismatch"), and fields dassie never reads went
        // straight through.
        let mut tar = Builder::new(Vec::new());
        let mut v7 = Header::new_old();
        v7.set_entry_type(EntryType::Regular);
        v7.set_mode(0o100_755);
        v7.set_uid(7);
        v7.set_size(1);
        tar.append_data(&mut v7, "v7", b"x".as_slice()).unwrap();
        let mut odd = Layer::header(EntryType::Regular, 0o644, 0, 0, 1);
        odd.as_ustar_mut().unwrap().uname[..4].copy_from_slice(b"evil");
        odd.as_mut_bytes()[257..265].copy_from_slice(b"star  \0\0");
        tar.append_data(&mut odd, "odd", b"y".as_slice()).unwrap();
        let layer = tar.into_inner().unwrap();

        let mut tree = Tree::new();
        tree.scan(layer.as_slice()).unwrap();
        tree.finish().unwrap();
        let out = tree
            .emit(|_| Ok(layer.as_slice()), b"{}", Vec::new())
            .unwrap();
        let mut archive = Archive::new(out.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let gnu = entry.header().as_gnu().expect("a GNU header");
            assert_eq!((gnu.uname, gnu.gname), ([0; 32], [0; 32]));
            seen.push((
                entry.path().unwrap().to_string_lossy().into_owned(),
                entry.header().mode().unwrap(),
                entry.header().uid().unwrap(),
            ));
        }
        // Mode type bits are dropped.
        assert_eq!(
            seen[1..3],
            [("v7".into(), 0o755, 7), ("odd".into(), 0o644, 0)]
        );
    }

    #[test]
    fn pax_numbers_replace_header_fields() {
        let layer = with_pax(
            EntryType::Regular,
            &[
                ("uid", b"70000"),
                ("gid", b"5"),
                ("mtime", b"1700000000.25"),
            ],
        );
        let out = flatten(&[layer], b"{}").unwrap();
        let f = find(&out, "f");
        assert_eq!((f.uid, f.gid, f.mtime), (70000, 5, 1_700_000_000));

        let before_epoch = with_pax(EntryType::Regular, &[("mtime", b"-5.5")]);
        let out = flatten(&[before_epoch], b"{}").unwrap();
        assert_eq!(find(&out, "f").mtime, 0);

        for (key, value) in [
            ("uid", &b"4294967296"[..]),
            ("gid", b"x"),
            ("mtime", b"1.x"),
        ] {
            let layer = with_pax(EntryType::Regular, &[(key, value)]);
            let err = flatten(&[layer], b"{}").unwrap_err();
            assert!(
                matches!(err, Error::BadField { field, .. } if field == key),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn only_inode_entry_types_are_carried_over() {
        for ty in *b"VMDZ" {
            let layer = with_pax(EntryType::new(ty), &[]);
            let err = flatten(&[layer], b"{}").unwrap_err();
            assert!(
                matches!(err, Error::Unsupported(_, t) if t.as_byte() == ty),
                "{err}"
            );
        }
        let fifo = with_pax(EntryType::Fifo, &[]);
        assert_eq!(
            find(&flatten(&[fifo], b"{}").unwrap(), "f").kind,
            EntryType::Fifo
        );
    }

    #[test]
    fn xattrs_erofs_would_drop_are_dropped() {
        // Found by the flatten_erofs fuzz target: mkfs.erofs drops unknown
        // namespaces (`system.posix_acl_ty` among them), and the kernel will
        // not read a malformed capability or ACL.
        let layer = with_pax(
            EntryType::Regular,
            &[
                ("SCHILY.xattr.user.keep", b"1"),
                ("SCHILY.xattr.trusted.empty", b""),
                ("SCHILY.xattr.com.apple.provenance", b"2"),
                ("SCHILY.xattr.user.", b"3"),
                ("SCHILY.xattr.security.capability", b"\x01"),
                ("SCHILY.xattr.system.posix_acl_ty", b"\x01"),
                ("SCHILY.xattr.system.posix_acl_access", b"\x02\0\0\0"),
                ("SCHILY.xattr.system.posix_acl_default", b"\x01"),
                ("LIBARCHIVE.xattr.user.b64", b"NA=="),
                ("atime", b"1"),
            ],
        );
        let out = flatten(&[layer], b"{}").unwrap();
        let keys: Vec<_> = find(&out, "f")
            .xattrs
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(
            keys,
            ["SCHILY.xattr.trusted.empty", "SCHILY.xattr.user.keep"]
        );

        // `user::rw-`, `group::r--`, `other::r--`, as setfacl writes it.
        let acl = [
            2, 0, 0, 0, 1, 0, 6, 0, 0xff, 0xff, 0xff, 0xff, 4, 0, 4, 0, 0xff, 0xff, 0xff, 0xff,
            0x20, 0, 4, 0, 0xff, 0xff, 0xff, 0xff,
        ];
        let layer = with_pax(
            EntryType::Regular,
            &[
                ("SCHILY.xattr.security.capability", CAP.as_slice()),
                ("SCHILY.xattr.system.posix_acl_access", acl.as_slice()),
            ],
        );
        let out = flatten(&[layer], b"{}").unwrap();
        assert_eq!(find(&out, "f").xattrs.len(), 2);
    }

    #[test]
    fn the_last_xattr_record_wins_before_filtering() {
        // mkfs.erofs keeps the last value for a key, so a malformed last
        // capability means none, whatever came first.
        for (records, kept) in [
            ([CAP.as_slice(), b"\x01"], 0),
            ([b"\x01".as_slice(), CAP.as_slice()], 1),
        ] {
            let pax: Vec<_> = records
                .iter()
                .map(|v| ("SCHILY.xattr.security.capability", *v))
                .collect();
            let out = flatten(&[with_pax(EntryType::Regular, &pax)], b"{}").unwrap();
            assert_eq!(find(&out, "f").xattrs.len(), kept);
        }
    }

    #[test]
    fn symlink_targets_are_written_byte_for_byte() {
        let long = format!("{}/./x/", "a".repeat(120));
        let short_cleaned = format!("{}x", "./".repeat(60));
        for target in ["f/.", "./f", "d//f/", short_cleaned.as_str(), long.as_str()] {
            // `Builder::append_link` would clean the fixture's target too.
            let mut tar = Builder::new(Vec::new());
            let mut h = Layer::header(EntryType::Symlink, 0o777, 0, 0, 0);
            append_symlink(&mut tar, &mut h, Path::new("l"), target.as_bytes()).unwrap();
            let layer = tar.into_inner().unwrap();
            let out = flatten(&[layer], b"{}").unwrap();
            assert_eq!(find(&out, "l").link, target);
        }
    }
}
