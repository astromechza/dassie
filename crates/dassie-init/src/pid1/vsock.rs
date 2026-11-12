//! vsock transports for the supervisor: the events connector and the control
//! listener (ADR 0004).
//!
//! Connections are wrapped in [`File`], which reads and writes the socket fd
//! with plain `read(2)`/`write(2)`; that is all the framing needs.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use dassie_init::control::ControlListener;
use nix::libc;
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept4, bind, connect as vconnect,
    listen, socket,
};

use super::Result;

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
pub(super) fn connect(cid: u32, port: u32) -> io::Result<Box<dyn Write + Send>> {
    let fd = stream()?;
    vconnect(fd.as_raw_fd(), &VsockAddr::new(cid, port))?;
    Ok(Box::new(File::from(fd)))
}

/// Accepts host-initiated connections on a guest port.
#[derive(Debug)]
pub(super) struct Listener(OwnedFd);

impl Listener {
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

impl ControlListener for Listener {
    fn accept(&self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let raw = accept4(self.0.as_raw_fd(), SockFlag::SOCK_CLOEXEC)?;
        // SAFETY: accept4 just returned this fd, and nothing else owns it.
        let conn = File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        Ok((Box::new(conn.try_clone()?), Box::new(conn)))
    }
}
