//! Brings network interfaces up. Addresses come from the kernel: loopback
//! gets 127.0.0.1 when it goes up, and `eth0` gets its address from the
//! `ip=` cmdline arg during boot.

use std::os::fd::AsRawFd;
use std::{io, mem};

use nix::libc;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};

use super::Result;

/// Sets `IFF_UP` on the interface, keeping its other flags.
pub(super) fn up(name: &str) -> Result<()> {
    set_up(name).map_err(|e| format!("bring up {name}: {e}").into())
}

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
