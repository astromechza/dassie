//! Write-to-temp-then-rename, so a crash never leaves a partial file at the
//! final path. Shared by the rootfs build and the blob store.
//!
//! A process killed mid-write (SIGKILL, OOM, power loss) still leaves its
//! temporary file behind; [`sweep`] removes those once they are old enough.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How old a temporary file must be before [`sweep`] removes it. Far longer
/// than any write takes, so a pull or build still running in another process
/// is never touched. Age, not a live-pid check: pids are reused, and the
/// store may be shared between hosts.
pub(crate) const STALE_AFTER: Duration = Duration::from_hours(1);

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

/// Whether `name` is one of [`temp_for`]'s: `.<name>.tmp.<pid>.<seq>`.
fn is_temp(name: &OsStr) -> bool {
    let Some(rest) = name.to_str().and_then(|n| n.strip_prefix('.')) else {
        return false;
    };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mut parts = rest.rsplitn(4, '.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(seq), Some(pid), Some("tmp"), Some(base))
            if number(seq) && number(pid) && !base.is_empty()
    )
}

/// Removes the temporary files in `dir` (not its subdirectories) that were
/// last modified more than `grace` ago, and returns their paths. Other files,
/// and temporary files modified recently or in the future, are left alone. A
/// missing `dir` has nothing to sweep.
pub(crate) fn sweep(dir: &Path, grace: Duration) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        r => r?,
    };
    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !is_temp(&entry.file_name()) {
            continue;
        }
        // Not following symlinks: only regular files are ever written here.
        let meta = entry.metadata()?;
        // `elapsed` fails for an mtime in the future: treat that as fresh.
        let stale = meta.modified()?.elapsed().is_ok_and(|age| age > grace);
        if !meta.is_file() || !stale {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed.push(entry.path()),
            // Another sweeper got there first.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn touch(path: &Path, age: Duration) {
        let file = File::create(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn recognizes_only_its_own_temporary_names() {
        let tmp = temp_for(Path::new("/x/index.json"));
        assert!(is_temp(tmp.file_name().unwrap()));
        for name in [".abc.tmp.12.3", ".index.json.tmp.1.0"] {
            assert!(is_temp(OsStr::new(name)), "{name}");
        }
        for name in [
            "abc.tmp.12.3",
            "..tmp.12.3",
            ".abc.tmp.12",
            ".abc.tmp.x.3",
            ".abc.tmp.12.",
            ".abc.temp.12.3",
            ".keep",
        ] {
            assert!(!is_temp(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn sweeps_stale_temporary_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let hour = Duration::from_hours(1);
        let old = dir.path().join(".blob.tmp.1.0");
        let fresh = dir.path().join(".blob.tmp.2.0");
        let future = dir.path().join(".blob.tmp.3.0");
        let dotfile = dir.path().join(".keep");
        let blob = dir.path().join("blob");
        touch(&old, 2 * hour);
        touch(&fresh, Duration::ZERO);
        File::create(&future)
            .unwrap()
            .set_modified(SystemTime::now() + hour)
            .unwrap();
        touch(&dotfile, 2 * hour);
        touch(&blob, 2 * hour);
        // A directory with a temporary name is not ours to remove.
        let subdir = dir.path().join(".dir.tmp.4.0");
        fs::create_dir(&subdir).unwrap();

        assert_eq!(sweep(dir.path(), hour).unwrap(), vec![old.clone()]);
        assert!(!old.exists());
        for kept in [fresh, future, dotfile, blob, subdir] {
            assert!(kept.exists(), "{}", kept.display());
        }
        assert_eq!(sweep(dir.path(), hour).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(
            sweep(&dir.path().join("missing"), hour).unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
