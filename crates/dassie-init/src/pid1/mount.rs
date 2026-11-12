//! Early mounts and the switch from the initramfs into the app rootfs
//! (ADR 0006).

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use dassie_init::boot::RootSpec;
use nix::errno::Errno;
use nix::libc;
use nix::mount::{MsFlags, mount};
use nix::unistd::{chdir, chroot, dup2_stderr, dup2_stdin, dup2_stdout};

use super::Result;

const NEW_ROOT: &str = "/newroot";
/// Mounts carried over from the initramfs into the app rootfs.
const MOVED: [&str; 3] = ["/dev", "/proc", "/sys"];
/// How long to wait for the root drive's device node to appear.
const ROOT_WAIT: Duration = Duration::from_secs(5);

/// Mounts devtmpfs, proc and sysfs in the initramfs.
pub(super) fn early() -> Result<()> {
    let nosuid = MsFlags::MS_NOSUID;
    let pseudo = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    mount_fs("devtmpfs", "/dev", nosuid)?;
    mount_fs("proc", "/proc", pseudo)?;
    mount_fs("sysfs", "/sys", pseudo)?;
    Ok(())
}

fn mount_fs(fstype: &str, target: &str, flags: MsFlags) -> Result<()> {
    mkdir(target)?;
    match mount(Some(fstype), target, Some(fstype), flags, None::<&str>) {
        // Already mounted, e.g. devtmpfs by CONFIG_DEVTMPFS_MOUNT.
        Ok(()) | Err(Errno::EBUSY) => Ok(()),
        Err(e) => Err(format!("mount {fstype} on {target}: {e}").into()),
    }
}

/// Points stdin, stdout and stderr at `/dev/console`, which now comes from
/// devtmpfs. The entrypoint inherits stdout and stderr, so its output reaches
/// the serial console.
pub(super) fn reopen_console() -> Result<()> {
    // std's runtime guarantees fds 0-2 are open before `main`, so this fd is
    // at least 3 and dropping it leaves the duplicates in place. dup2 clears
    // close-on-exec on the targets.
    let console = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/console")
        .map_err(|e| format!("open /dev/console: {e}"))?;
    dup2_stdin(&console)
        .and_then(|()| dup2_stdout(&console))
        .and_then(|()| dup2_stderr(&console))
        .map_err(|e| format!("dup /dev/console: {e}"))?;
    Ok(())
}

/// Mounts the root drive at `/newroot`, moves `/dev`, `/proc` and `/sys`
/// into it, and makes it `/`, as busybox `switch_root` does. The initramfs
/// is a rootfs mount, which `pivot_root` cannot leave, so this moves the new
/// root over `/` and chroots into it. PID 1 keeps running throughout.
pub(super) fn switch_root(root: &RootSpec) -> Result<()> {
    wait_for(&root.device)?;
    mkdir(NEW_ROOT)?;
    let mut flags = MsFlags::empty();
    if root.read_only {
        flags |= MsFlags::MS_RDONLY;
    }
    mount(
        Some(root.device.as_str()),
        NEW_ROOT,
        Some(root.fstype.as_str()),
        flags,
        None::<&str>,
    )
    .map_err(|e| format!("mount {} ({}) on {NEW_ROOT}: {e}", root.device, root.fstype))?;

    for dir in MOVED {
        let target = format!("{NEW_ROOT}{dir}");
        mkdir(&target)?;
        mount(
            Some(dir),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_MOVE,
            None::<&str>,
        )
        .map_err(|e| format!("move {dir} to {target}: {e}"))?;
    }

    chdir(NEW_ROOT).map_err(|e| format!("chdir {NEW_ROOT}: {e}"))?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .map_err(|e| format!("move {NEW_ROOT} to /: {e}"))?;
    chroot(".").map_err(|e| format!("chroot: {e}"))?;
    chdir("/").map_err(|e| format!("chdir /: {e}"))?;
    Ok(())
}

/// Creates a directory if it is missing. In the app rootfs this needs a
/// writable mount, so a read-only rootfs must already have the directories.
fn mkdir(path: &str) -> Result<()> {
    if Path::new(path).is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .mode(0o755)
        .create(path)
        .map_err(|e| format!("mkdir {path}: {e}").into())
}

/// Waits for devtmpfs to show the device node, in case the drive is probed
/// after init starts.
fn wait_for(device: &str) -> Result<()> {
    let deadline = Instant::now() + ROOT_WAIT;
    while !Path::new(device).exists() {
        if Instant::now() >= deadline {
            return Err(format!("root device {device} did not appear").into());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
