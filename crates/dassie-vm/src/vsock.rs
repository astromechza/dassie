//! The host side of Firecracker's vsock device (ADR 0004).
//!
//! Firecracker exposes the guest's vsock as Unix sockets:
//!
//! - A guest connection to the host (CID 2) on port P arrives on
//!   `<uds_path>_P`, which the host must be listening on: [`guest_port_path`].
//! - A host connection to guest port P goes to `<uds_path>`, starting with
//!   `CONNECT P\n` and Firecracker's `OK <n>\n`: [`ControlClient`] does this
//!   for the control port.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use std::{fmt, fs, thread};

use dassie_proto::{CONTROL_PORT, FrameReader, FrameWriter, Request, Response};

/// How long to wait when probing whether an existing socket is live.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Longest handshake reply read from Firecracker (`OK <u32>\n` fits easily).
const MAX_ACK: u64 = 64;

/// The host socket Firecracker connects to when the guest connects to the
/// host on `port`: `<uds_path>_<port>`.
pub fn guest_port_path(uds: &Path, port: u32) -> PathBuf {
    let mut path = uds.as_os_str().to_owned();
    path.push(format!("_{port}"));
    PathBuf::from(path)
}

/// Why a control request failed.
#[derive(Debug)]
pub enum ControlError {
    /// Connecting to `<uds_path>`, or the handshake's I/O, failed.
    Io(io::Error),
    /// Firecracker did not answer `OK`: nothing listens on the guest port
    /// (yet), and Firecracker closed the connection.
    Refused(String),
    /// Sending the request or reading the response failed.
    Proto(dassie_proto::Error),
    /// The guest closed the connection without a response.
    Closed,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Refused(ack) => write!(
                f,
                "handshake refused ({ack:?}); is the guest listening on port {CONTROL_PORT}?"
            ),
            Self::Proto(e) => write!(f, "{e}"),
            Self::Closed => write!(f, "guest closed the connection without a response"),
        }
    }
}

impl std::error::Error for ControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Proto(e) => Some(e),
            Self::Refused(_) | Self::Closed => None,
        }
    }
}

impl From<io::Error> for ControlError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A connection to the guest's control channel, through Firecracker's vsock
/// socket. Carries any number of requests, one response each, in order.
#[derive(Debug)]
pub struct ControlClient {
    reader: FrameReader<BufReader<UnixStream>>,
    writer: FrameWriter<UnixStream>,
}

impl ControlClient {
    /// Connects to the guest's control port via `uds`, the vsock device's
    /// `uds_path`. `timeout` bounds every read and write, the handshake's
    /// included.
    pub fn connect(uds: &Path, timeout: Duration) -> Result<Self, ControlError> {
        let stream = UnixStream::connect(uds)
            .map_err(|e| io::Error::new(e.kind(), format!("connect {}: {e}", uds.display())))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;

        // Firecracker's host-initiated handshake: `CONNECT <port>\n` -> `OK <n>\n`.
        writeln!(writer, "CONNECT {CONTROL_PORT}")?;
        let mut ack = String::new();
        Read::by_ref(&mut reader)
            .take(MAX_ACK)
            .read_line(&mut ack)?;
        if !ack.starts_with("OK ") {
            return Err(ControlError::Refused(ack));
        }
        // Nothing can be buffered past the ack: the guest only speaks when
        // asked.
        Ok(Self {
            reader: FrameReader::new(reader),
            writer: FrameWriter::new(writer),
        })
    }

    /// Sends one request and waits for its response.
    pub fn request(&mut self, request: &Request) -> Result<Response, ControlError> {
        self.writer.send(request).map_err(ControlError::Proto)?;
        self.reader
            .recv::<Response>()
            .map_err(ControlError::Proto)?
            .ok_or(ControlError::Closed)
    }
}

/// Removes a socket left behind by a previous run, such as a killed
/// Firecracker's `<uds_path>` or a host listener's `<uds_path>_1024`.
///
/// Does nothing if `path` does not exist. Refuses a path that is not a
/// socket, or that something is still listening on.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let shown = path.display();
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("stat {shown}: {e}"))),
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{shown} exists and is not a socket"),
            ));
        }
        Ok(_) => {}
    }
    // Only a refused connection proves nobody is listening; any other error
    // (e.g. permission denied) leaves the socket alone. On Linux a connect to
    // a live socket with a full accept queue blocks, so bound the probe.
    let (tx, rx) = mpsc::channel();
    let probe_path = path.to_owned();
    thread::spawn(move || {
        // If the receiver already timed out, dropping the result closes the stream.
        drop(tx.send(UnixStream::connect(probe_path)));
    });
    let probe = rx.recv_timeout(PROBE_TIMEOUT).map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("probe {shown}: timed out; is another listener busy?"),
        )
    })?;
    match probe {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{shown} is in use by another listener"),
        )),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path)
            .map_err(|e| io::Error::new(e.kind(), format!("remove stale {shown}: {e}"))),
        Err(e) => Err(io::Error::new(e.kind(), format!("probe {shown}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use dassie_proto::{GuestState, HealthStatus};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Plays Firecracker's side of one host-initiated connection: reads the
    /// `CONNECT` line, then either refuses or acks and answers each request
    /// with `respond`.
    fn fake_firecracker(
        listener: UnixListener,
        ack: Option<&'static str>,
        respond: fn(Request) -> Response,
    ) -> thread::JoinHandle<String> {
        thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let Some(ack) = ack else {
                return line;
            };
            (&conn).write_all(ack.as_bytes()).unwrap();
            let mut frames = FrameReader::new(reader);
            let mut writer = FrameWriter::new(conn);
            while let Some(request) = frames.recv::<Request>().unwrap() {
                writer.send(&respond(request)).unwrap();
            }
            line
        })
    }

    fn ready(_: Request) -> Response {
        Response::Health(HealthStatus {
            state: GuestState::Ready,
            pid: 7,
        })
    }

    #[test]
    fn guest_port_path_appends_the_port() {
        assert_eq!(
            guest_port_path(Path::new("/run/dassie/a.vsock"), 1024),
            Path::new("/run/dassie/a.vsock_1024")
        );
    }

    #[test]
    fn sends_requests_after_the_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("vm.vsock");
        let fake = fake_firecracker(
            UnixListener::bind(&uds).unwrap(),
            Some("OK 1073741824\n"),
            ready,
        );

        let mut client = ControlClient::connect(&uds, TIMEOUT).unwrap();
        for request in [
            Request::Health,
            Request::Stop {
                timeout_ms: Some(5),
            },
        ] {
            assert_eq!(client.request(&request).unwrap(), ready(request));
        }
        drop(client);
        assert_eq!(fake.join().unwrap(), "CONNECT 1025\n");
    }

    #[test]
    fn reports_a_refused_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("vm.vsock");
        let fake = fake_firecracker(UnixListener::bind(&uds).unwrap(), None, ready);

        let err = ControlClient::connect(&uds, TIMEOUT).unwrap_err();
        assert!(
            matches!(&err, ControlError::Refused(ack) if ack.is_empty()),
            "{err}"
        );
        fake.join().unwrap();
    }

    #[test]
    fn reports_a_missing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let err = ControlClient::connect(&dir.path().join("vm.vsock"), TIMEOUT).unwrap_err();
        assert!(
            matches!(&err, ControlError::Io(e) if e.kind() == io::ErrorKind::NotFound),
            "{err}"
        );
    }

    #[test]
    fn removes_only_stale_sockets() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("missing");
        remove_stale_socket(&missing).unwrap();

        // A listener that is gone leaves its socket file behind.
        let stale = dir.path().join("stale");
        drop(UnixListener::bind(&stale).unwrap());
        assert!(stale.exists());
        remove_stale_socket(&stale).unwrap();
        assert!(!stale.exists());

        let live = dir.path().join("live");
        let _listener = UnixListener::bind(&live).unwrap();
        let err = remove_stale_socket(&live).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(live.exists());

        let file = dir.path().join("file");
        fs::write(&file, b"").unwrap();
        let err = remove_stale_socket(&file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(file.exists());
    }
}
