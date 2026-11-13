//! Write-to-temp-then-rename, so a crash never leaves a partial file at the
//! final path. Shared by the rootfs build and the blob store.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Numbers each temporary file, so concurrent writers of one path in one
/// process never share it. The pid separates processes.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temporary name next to `path`: `.<name>.tmp.<pid>.<seq>`. Same
/// directory, so the final rename never crosses filesystems.
pub(crate) fn temp_for(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    name.push(format!(".tmp.{}.{seq}", std::process::id()));
    path.with_file_name(name)
}

/// Flushes `tmp` to disk, renames it to `path` and syncs the directory, so
/// the new name survives a power cut.
pub(crate) fn commit(tmp: &Path, path: &Path) -> io::Result<()> {
    File::open(tmp)?.sync_all()?;
    fs::rename(tmp, path)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(dir)?.sync_all()
}

/// Runs `write` on a fresh temporary path next to `path`, then commits it.
/// On any error the temporary file is removed and `path` is untouched.
/// `io_err` wraps a commit failure (an `Error::Io(path, e)` variant).
pub(crate) fn write_with<T, E>(
    path: &Path,
    io_err: fn(PathBuf, io::Error) -> E,
    write: impl FnOnce(&Path) -> Result<T, E>,
) -> Result<T, E> {
    let tmp = temp_for(path);
    let result = write(&tmp).and_then(|v| {
        commit(&tmp, path).map_err(|e| io_err(tmp.clone(), e))?;
        Ok(v)
    });
    if result.is_err() {
        drop(fs::remove_file(&tmp));
    }
    result
}
