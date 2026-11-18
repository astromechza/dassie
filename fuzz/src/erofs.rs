//! A differential oracle for the flattener: feed its output to the real
//! `mkfs.erofs --tar=f`, mount the image read-only through the kernel's erofs
//! driver (the guest's own reader), and check it holds the tree the `tar`
//! crate reads from the same stream.
//!
//! The flattener checks a layer as the `tar` crate parses it, but the guest
//! sees what mkfs.erofs parses. Every field the two read differently is a
//! place where the filesystem can differ from the tree dassie checked
//! (ADR 0007). [`check_output`](crate::check_output) cannot see those: it
//! uses the same parser as the writer.
//!
//! Needs root (to loop-mount, and to read `trusted.*` xattrs), the erofs
//! kernel module and erofs-utils on `PATH`. `fsck.erofs --extract` would
//! avoid the mount, but it cannot read empty xattr values and the kernel
//! refuses to set some (a malformed `security.capability`), so it is not a
//! faithful reader.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::Command;

use tar::{Archive, EntryType};

use crate::{Kind, normalise, show};

/// One inode as both sides see it.
#[derive(Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    /// Permission bits, setuid/setgid/sticky included. `None` for symlinks,
    /// which Linux always shows as `0777`.
    pub mode: Option<u32>,
    pub uid: Option<u64>,
    pub gid: Option<u64>,
    /// Whole seconds.
    pub mtime: Option<i64>,
    /// `(major, minor)`, for character and block devices only.
    pub rdev: Option<(u64, u64)>,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// The smallest path sharing this inode, so hard link groups compare
    /// equal however they were found.
    pub group: Vec<u8>,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            Kind::File(d) => format!("File({})", show(d)),
            Kind::Symlink(t) => format!("Symlink({})", show(t)),
            Kind::Hardlink(t) => format!("Hardlink({})", show(t)),
            k => format!("{k:?}"),
        };
        let xattrs: Vec<_> = self
            .xattrs
            .iter()
            .map(|(k, v)| format!("{}={}", show(k), v.escape_ascii()))
            .collect();
        f.debug_struct("Node")
            .field("kind", &format_args!("{kind}"))
            .field("mode", &self.mode.map(|m| format!("{m:o}")))
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("mtime", &self.mtime)
            .field("rdev", &self.rdev)
            .field("xattrs", &xattrs)
            .field("group", &format_args!("{}", show(&self.group)))
            .finish()
    }
}

/// Every inode by normalised path, the root as `""`.
pub type View = BTreeMap<Vec<u8>, Node>;

/// Panics unless mkfs.erofs builds `out` (a flattened stream that passed
/// [`check_output`](crate::check_output)) into the tree the `tar` crate
/// reads from it.
pub fn check_erofs(out: &[u8]) {
    let want = tar_view(out);
    let got = erofs_view(out);
    if want != got {
        let mut diff = String::new();
        for path in want.keys().chain(got.keys()).collect::<BTreeSet<_>>() {
            let (w, g) = (want.get(path), got.get(path));
            if w != g {
                diff += &format!("\n  {:?}\n    tar:   {w:?}\n    erofs: {g:?}", show(path));
            }
        }
        panic!("mkfs.erofs built a different tree:{diff}");
    }
}

/// The tree as the `tar` crate reads `out`, with PAX `uid`, `gid` and
/// `mtime` records applied over the header fields, as POSIX says.
pub fn tar_view(out: &[u8]) -> View {
    let mut view = View::new();
    let mut links = Vec::new();
    let mut archive = Archive::new(out);
    for entry in archive.entries().expect("output is a tar stream") {
        let mut entry = entry.expect("output entry parses");
        let path = normalise(&entry.path_bytes());
        let mut pax = BTreeMap::new();
        if let Some(exts) = entry.pax_extensions().expect("PAX records parse") {
            for ext in exts {
                let ext = ext.expect("PAX record parses");
                pax.insert(ext.key_bytes().to_vec(), ext.value_bytes().to_vec());
            }
        }
        let header = entry.header().clone();
        let ty = header.entry_type();
        let kind = match ty {
            EntryType::Directory => Kind::Dir,
            EntryType::Regular | EntryType::Continuous => {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).expect("output entry data");
                Kind::File(data)
            }
            EntryType::Symlink => Kind::Symlink(link_name(&entry)),
            EntryType::Link => {
                links.push((path, normalise(&link_name(&entry))));
                continue;
            }
            other => Kind::Other(other.as_byte()),
        };
        let number = |key: &[u8], field: Option<u64>| match pax.get(key) {
            Some(v) => std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()),
            None => field,
        };
        let mtime = match pax.get(&b"mtime"[..]) {
            Some(v) => std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.split('.').next()?.parse().ok()),
            None => header.mtime().ok().and_then(|m| i64::try_from(m).ok()),
        };
        let xattrs = pax
            .iter()
            .filter_map(|(k, v)| Some((k.strip_prefix(b"SCHILY.xattr.")?.to_vec(), v.clone())))
            .collect();
        let rdev = matches!(ty, EntryType::Char | EntryType::Block).then(|| {
            (
                header
                    .device_major()
                    .ok()
                    .flatten()
                    .map_or(u64::MAX, u64::from),
                header
                    .device_minor()
                    .ok()
                    .flatten()
                    .map_or(u64::MAX, u64::from),
            )
        });
        let node = Node {
            mode: (ty != EntryType::Symlink)
                .then(|| header.mode().ok().map(|m| m & 0o7777))
                .flatten(),
            uid: number(b"uid", header.uid().ok()),
            gid: number(b"gid", header.gid().ok()),
            mtime,
            rdev,
            xattrs,
            group: path.clone(),
            kind,
        };
        view.insert(path, node);
    }
    // A hard link is another name for its target's inode: the link's own
    // header fields do not apply. `check_output` has checked the target
    // comes first and is not a directory.
    for (path, target) in links {
        let node = view[&target].clone();
        view.insert(path, node);
    }
    regroup(&mut view);
    view
}

/// The tree mkfs.erofs builds from `out`, as the kernel mounts it.
pub fn erofs_view(out: &[u8]) -> View {
    let uid = fs::metadata("/proc/self").expect("/proc/self").uid();
    assert_eq!(uid, 0, "the erofs oracle needs root");
    let dir = tempfile::tempdir().expect("tempdir");
    let tar = dir.path().join("in.tar");
    let image = dir.path().join("img");
    let root = dir.path().join("mnt");
    fs::write(&tar, out).expect("write tar");
    fs::create_dir(&root).expect("mkdir mnt");
    run(Command::new("mkfs.erofs")
        .args(["--quiet", "--tar=f"])
        .arg(&image)
        .arg(&tar));
    run(Command::new("mount")
        .args(["-t", "erofs", "-o", "loop,ro"])
        .arg(&image)
        .arg(&root));
    // Read everything before unmounting: a panic in between would leave
    // the mount behind.
    let walked = std::panic::catch_unwind(|| {
        let mut view = View::new();
        walk(&root, &root, &mut view);
        view
    });
    run(Command::new("umount").arg(&root));
    let mut view = walked.unwrap_or_else(|e| std::panic::resume_unwind(e));
    regroup(&mut view);
    view
}

/// Adds `path` and everything under it to `view`. Until [`regroup`],
/// `group` holds the inode number, or the path for a directory.
fn walk(root: &Path, path: &Path, view: &mut View) {
    let meta = fs::symlink_metadata(path).expect("lstat");
    let ft = meta.file_type();
    let kind = if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File(fs::read(path).expect("read file"))
    } else if ft.is_symlink() {
        let target = fs::read_link(path).expect("readlink");
        Kind::Symlink(target.as_os_str().as_bytes().to_vec())
    } else if ft.is_char_device() {
        Kind::Other(EntryType::Char.as_byte())
    } else if ft.is_block_device() {
        Kind::Other(EntryType::Block.as_byte())
    } else if ft.is_fifo() {
        Kind::Other(EntryType::Fifo.as_byte())
    } else {
        Kind::Other(0)
    };
    let rdev = (ft.is_char_device() || ft.is_block_device()).then(|| {
        let r = meta.rdev();
        (u64::from(dev_major(r)), u64::from(dev_minor(r)))
    });
    let xattrs = xattr::list(path)
        .expect("list xattrs")
        .map(|name| {
            // An xattr the kernel lists but will not read is a divergence
            // too, so it shows in the diff rather than panicking here.
            let value = match xattr::get(path, &name) {
                Ok(v) => v.unwrap_or_default(),
                Err(e) => format!("<getxattr failed: {e}>").into_bytes(),
            };
            (name.as_bytes().to_vec(), value)
        })
        .collect();
    let rel = path.strip_prefix(root).expect("under root");
    view.insert(
        rel.as_os_str().as_bytes().to_vec(),
        Node {
            kind,
            mode: (!ft.is_symlink()).then_some(meta.mode() & 0o7777),
            uid: Some(u64::from(meta.uid())),
            gid: Some(u64::from(meta.gid())),
            mtime: Some(meta.mtime()),
            rdev,
            xattrs,
            // Directories are never hard linked.
            group: if ft.is_dir() {
                [b"d:", rel.as_os_str().as_bytes()].concat()
            } else {
                format!("i:{}", meta.ino()).into_bytes()
            },
        },
    );
    if ft.is_dir() {
        for entry in fs::read_dir(path).expect("read dir") {
            walk(root, &entry.expect("dir entry").path(), view);
        }
    }
}

/// Replaces each node's `group` key with the smallest path sharing it.
fn regroup(view: &mut View) {
    let mut smallest: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (path, node) in view.iter() {
        // `view` iterates in path order, so the first path wins.
        smallest
            .entry(node.group.clone())
            .or_insert_with(|| path.clone());
    }
    for node in view.values_mut() {
        node.group = smallest[&node.group].clone();
    }
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?} failed on a stream the flattener accepted: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn link_name<R: Read>(entry: &tar::Entry<'_, R>) -> Vec<u8> {
    entry
        .link_name_bytes()
        .map(|l| l.into_owned())
        .unwrap_or_default()
}

// glibc's `gnu_dev_major`/`gnu_dev_minor`, without a libc dependency.
fn dev_major(dev: u64) -> u32 {
    (((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0x0000_0fff)) as u32
}

fn dev_minor(dev: u64) -> u32 {
    (((dev >> 12) & 0xffff_ff00) | (dev & 0x0000_00ff)) as u32
}
