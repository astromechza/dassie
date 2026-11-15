//! Builds a small erofs from files in memory, such as the guest drive
//! (ADR 0015), with the same deterministic `mkfs.erofs` run as image
//! artefacts.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use tar::{Builder, EntryType};

use crate::atomic;
use crate::build::{Error, Options, run_mkfs};
use crate::digest::Digest;
use crate::flatten::{ROOT_MODE, owned_header};

/// One file at the root of the filesystem, owned by root.
#[derive(Debug, Clone, Copy)]
pub struct FileEntry<'a> {
    /// A plain file name: no `/`, and not `.` or `..`.
    pub name: &'a str,
    /// Permission bits, such as `0o400`.
    pub mode: u32,
    pub contents: &'a [u8],
}

/// Writes an uncompressed erofs holding `files` to `out`, which is replaced
/// atomically and left `0600`, since it may hold per-VM values: the caller
/// widens it for whoever must read it. The root is `0755 root` and every file is
/// owned by root with a zero mtime. The superblock time is fixed and the
/// UUID comes from the tar's sha256, so the same files always build the
/// same bytes.
pub fn build_files(files: &[FileEntry<'_>], out: &Path, opts: &Options) -> Result<(), Error> {
    let tar = tar_of(files).map_err(|e| Error::Io(out.to_path_buf(), e))?;
    let uuid = Digest::of(&tar).uuid();
    atomic::write_with(out, Error::Io, |tmp| {
        // Created `0600` up front, so the file is never readable by others
        // while mkfs writes it.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)
            .map_err(|e| Error::Io(tmp.to_path_buf(), e))?;
        run_mkfs(opts, tmp, &uuid, None, |mut stdin| {
            stdin
                .write_all(&tar)
                .and_then(|()| stdin.flush())
                .map_err(|e| Error::Io(opts.mkfs.clone(), e))
        })?;
        fs::set_permissions(tmp, fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::Io(tmp.to_path_buf(), e))
    })
}

fn tar_of(files: &[FileEntry<'_>]) -> io::Result<Vec<u8>> {
    let mut tar = Builder::new(Vec::new());
    // mkfs.erofs makes a root with no entry `0777`.
    let mut root = owned_header(EntryType::Directory, ROOT_MODE, 0);
    root.set_path("./")?;
    root.set_cksum();
    tar.append(&root, io::empty())?;
    for file in files {
        if file.name.contains('/') || matches!(file.name, "" | "." | "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{:?} is not a plain file name", file.name),
            ));
        }
        let size = u64::try_from(file.contents.len()).map_err(io::Error::other)?;
        let mut header = owned_header(EntryType::Regular, file.mode & 0o7777, size);
        tar.append_data(&mut header, file.name, file.contents)?;
    }
    tar.into_inner()
}

#[cfg(test)]
mod tests {
    use tar::Archive;

    use super::*;

    #[test]
    fn tar_has_a_root_then_the_files() {
        let tar = tar_of(&[FileEntry {
            name: "guest.json",
            mode: 0o400,
            contents: b"{}",
        }])
        .unwrap();
        let entries: Vec<_> = Archive::new(tar.as_slice())
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let h = e.header();
                (
                    e.path().unwrap().display().to_string(),
                    h.mode().unwrap(),
                    h.uid().unwrap(),
                    h.mtime().unwrap(),
                    h.size().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                ("./".to_owned(), 0o755, 0, 0, 0),
                ("guest.json".to_owned(), 0o400, 0, 0, 2),
            ]
        );
    }

    #[test]
    fn tar_is_deterministic_and_refuses_paths() {
        let file = |name| FileEntry {
            name,
            mode: 0o400,
            contents: b"x",
        };
        assert_eq!(tar_of(&[file("a")]).unwrap(), tar_of(&[file("a")]).unwrap());
        for name in ["", ".", "..", "a/b", "/a"] {
            assert!(tar_of(&[file(name)]).is_err(), "{name:?}");
        }
    }

    #[test]
    fn failed_mkfs_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("guest.drive");
        let opts = Options {
            mkfs: "false".into(),
        };
        assert!(matches!(
            build_files(&[], &out, &opts),
            Err(Error::Mkfs(..))
        ));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
