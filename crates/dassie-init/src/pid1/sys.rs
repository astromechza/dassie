//! The PID-1 layer's system calls: early mounts and the switch-root
//! (ADR 0006), interface bring-up, and the vsock transports (ADR 0004).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use dassie_init::boot::RootSpec;
use dassie_init::control::ControlListener;
use nix::errno::Errno;
use nix::libc;
use nix::mount::{MsFlags, mount};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept4, bind, connect, listen, socket,
};
use nix::unistd::{chdir, chroot, dup2_stderr, dup2_stdin, dup2_stdout};

use super::Result;

// Mounts and switch-root.

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

/// Adds what OCI runtimes provide in `/dev` beyond device nodes, which
/// devtmpfs does not create:
///
/// - `/dev/fd` and `/dev/std{in,out,err}`, linking to `/proc/self/fd`. Images
///   such as nginx link their log files to `/dev/stderr`.
/// - a tmpfs on `/dev/shm` for POSIX shared memory, mode `1777`, sized by the
///   kernel default (half the guest's RAM).
pub(super) fn dev_extras() -> Result<()> {
    let links = [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ];
    for (link, target) in links {
        match std::os::unix::fs::symlink(target, link) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!("symlink {link} -> {target}: {e}").into());
            }
            _ => {}
        }
    }
    mkdir("/dev/shm")?;
    let flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    match mount(
        Some("tmpfs"),
        "/dev/shm",
        Some("tmpfs"),
        flags,
        Some("mode=1777"),
    ) {
        Ok(()) | Err(Errno::EBUSY) => Ok(()),
        Err(e) => Err(format!("mount tmpfs on /dev/shm: {e}").into()),
    }
}

/// Makes the console, which is the entrypoint's stdout and stderr, belong to
/// the user it runs as. Without this, a non-root app cannot reopen its own
/// output through `/dev/stdout` (a link to `/dev/console`, mode `0600
/// root`). OCI runtimes do the same with a container's stdio.
pub(super) fn give_console(uid: u32, gid: u32) -> Result<()> {
    std::os::unix::fs::fchown(io::stdout(), Some(uid), Some(gid))
        .map_err(|e| format!("chown /dev/console to {uid}:{gid}: {e}").into())
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

// Network interfaces. Addresses come from the kernel: loopback gets
// 127.0.0.1 when it goes up, and `eth0` gets its address from the `ip=`
// cmdline arg during boot.

/// Sets `IFF_UP` on the interface, keeping its other flags.
pub(super) fn interface_up(name: &str) -> Result<()> {
    set_up(name).map_err(|e| format!("bring up {name}: {e}").into())
}

#[allow(unsafe_code)]
fn set_up(name: &str) -> io::Result<()> {
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    // SAFETY: `ifreq` is plain old data (a name array and a union of
    // integers and sockaddrs), for which all-zero bytes are valid.
    let mut req: libc::ifreq = unsafe { mem::zeroed() };
    let bytes = name.as_bytes();
    if bytes.len() >= req.ifr_name.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad interface name",
        ));
    }
    for (dst, &src) in req.ifr_name.iter_mut().zip(bytes) {
        *dst = libc::c_char::from_ne_bytes([src]);
    }

    ioctl(&sock, libc::SIOCGIFFLAGS, &mut req)?;
    // SAFETY: SIOCGIFFLAGS filled in the `ifru_flags` member.
    let flags = unsafe { req.ifr_ifru.ifru_flags };
    req.ifr_ifru.ifru_flags =
        flags | libc::c_short::try_from(libc::IFF_UP).map_err(io::Error::other)?;
    ioctl(&sock, libc::SIOCSIFFLAGS, &mut req)
}

#[allow(unsafe_code)]
fn ioctl(sock: &impl AsRawFd, request: libc::c_ulong, req: &mut libc::ifreq) -> io::Result<()> {
    let request = libc::Ioctl::try_from(request).map_err(io::Error::other)?;
    // SAFETY: both requests take a pointer to an `ifreq`, which `req` is for
    // the duration of the call, and the fd is an open socket.
    let res = unsafe { libc::ioctl(sock.as_raw_fd(), request, std::ptr::from_mut(req)) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// vsock transports for the supervisor: the events connector and the control
// listener. Connections are wrapped in [`File`], which reads and writes the
// socket fd with plain `read(2)`/`write(2)`; that is all the framing needs.

/// The host's CID as seen from the guest.
pub(super) const HOST_CID: u32 = libc::VMADDR_CID_HOST;

fn stream() -> io::Result<OwnedFd> {
    Ok(socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?)
}

/// Opens a connection to `port` on `cid`. Firecracker forwards it to the
/// host socket `<uds_path>_<port>`, and resets it if nothing listens there.
pub(super) fn vsock_connect(cid: u32, port: u32) -> io::Result<Box<dyn Write + Send>> {
    let fd = stream()?;
    connect(fd.as_raw_fd(), &VsockAddr::new(cid, port))?;
    Ok(Box::new(File::from(fd)))
}

/// Accepts host-initiated connections on a guest port.
#[derive(Debug)]
pub(super) struct VsockListener(OwnedFd);

impl VsockListener {
    pub(super) fn bind(port: u32) -> Result<Self> {
        let setup = || -> io::Result<OwnedFd> {
            let fd = stream()?;
            bind(fd.as_raw_fd(), &VsockAddr::new(libc::VMADDR_CID_ANY, port))?;
            listen(&fd, Backlog::new(8)?)?;
            Ok(fd)
        };
        Ok(Self(setup().map_err(|e| {
            format!("listen on vsock port {port}: {e}")
        })?))
    }
}

impl ControlListener for VsockListener {
    #[allow(unsafe_code)]
    fn accept(&self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let raw = accept4(self.0.as_raw_fd(), SockFlag::SOCK_CLOEXEC)?;
        // SAFETY: accept4 just returned this fd, and nothing else owns it.
        let conn = File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        Ok((Box::new(conn.try_clone()?), Box::new(conn)))
    }
}
