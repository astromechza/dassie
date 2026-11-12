//! Runs the `dassie-init` supervisor as an ordinary process, standing in for
//! the guest PID 1 and for Firecracker's vsock device, so the host <-> guest
//! contract can be exercised on any Unix without a VM. See `README.md` next
//! to this file for a walkthrough.
//!
//! ```text
//! supervise <uds_path> --root DIR [--cmdline "dassie.k=v ..."]
//! ```
//!
//! - The spec is loaded from `DIR/.dassie/image.json` (and `DIR/etc/passwd`,
//!   `DIR/etc/group` if present) plus the cmdline text, exactly as PID 1 will.
//! - Events go to `<uds_path>_1024`, where `listener events <uds_path>`
//!   listens, as Firecracker forwards guest-initiated vsock connections.
//! - Control connections are accepted on `<uds_path>` behind Firecracker's
//!   `CONNECT 1025` handshake, so `listener health|stop <uds_path>` work
//!   unchanged.
//! - SIGTERM or SIGINT to this process becomes a stop request, as PID 1 will
//!   handle them. The entrypoint is reaped with `waitpid(pid)`; PID 1 reaps
//!   everything with `waitpid(-1)`.

use std::error::Error;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use std::{env, fs, thread};

use dassie_init::config;
use dassie_init::control::ControlListener;
use dassie_init::supervisor::{self, Control, Deps};
use dassie_proto::{CONTROL_PORT, EVENTS_PORT};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

/// How long to wait when probing whether an existing socket is live.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

const USAGE: &str = "usage: supervise <uds_path> --root DIR [--cmdline \"dassie.k=v ...\"]";

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (uds, root, cmdline) = parse_args(&args).ok_or(USAGE)?;

    let spec = config::load(&root, &cmdline)?;
    eprintln!("supervise: {spec:#?}");

    // Block the stop signals before any thread starts so every thread
    // inherits the mask and only `sigwait` sees them, as PID 1 will. The
    // supervisor spawns the entrypoint with an empty mask.
    let mut stop_signals = SigSet::empty();
    stop_signals.add(Signal::SIGTERM);
    stop_signals.add(Signal::SIGINT);
    stop_signals.thread_block()?;

    if let Some(dir) = uds.parent() {
        fs::create_dir_all(dir)?;
    }
    remove_stale_socket(&uds)?;
    let control_listener = FirecrackerMux(UnixListener::bind(&uds)?);
    eprintln!("supervise: control on {}", uds.display());

    let events_path = PathBuf::from(format!("{}_{EVENTS_PORT}", uds.display()));
    let (control_tx, control_rx) = mpsc::channel();

    let signal_tx = control_tx.clone();
    thread::spawn(move || {
        while let Ok(sig) = stop_signals.wait() {
            eprintln!("supervise: {sig}, stopping");
            // Fails only once the supervisor is done; nothing left to stop.
            drop(signal_tx.send(Control::Stop {
                timeout: None,
                reply: None,
            }));
        }
    });

    let reaper_tx = control_tx.clone();
    let deps = Deps {
        connect: Box::new(move || {
            UnixStream::connect(&events_path).map(|s| Box::new(s) as Box<dyn Write + Send>)
        }),
        control_listener: Box::new(control_listener),
        control: (control_tx, control_rx),
        on_spawn: Box::new(move |pid| {
            eprintln!("supervise: spawned pid {pid}");
            thread::spawn(move || reap(pid, &reaper_tx));
        }),
    };

    let status = supervisor::run(&spec, deps)?;
    eprintln!("supervise: entrypoint exited with {status:?}; PID 1 would reboot now");
    Ok(())
}

fn parse_args(args: &[String]) -> Option<(PathBuf, PathBuf, String)> {
    let (uds, flags) = args.split_first()?;
    let mut root = None;
    let mut cmdline = String::new();
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(it.next()?)),
            "--cmdline" => it.next()?.clone_into(&mut cmdline),
            _ => return None,
        }
    }
    Some((PathBuf::from(uds), root?, cmdline))
}

/// Removes a socket left behind by a previous run. Refuses a path that is not
/// a socket or that another process is still serving; only a refused connect
/// proves the socket is stale.
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_socket() => {
            let msg = format!("{} exists and is not a socket", path.display());
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, msg));
        }
        Ok(_) => {}
    }
    // On Linux a connect to a live socket with a full accept queue blocks, so
    // bound the probe; a timeout counts as "in use".
    let (tx, rx) = mpsc::channel();
    let probe_path = path.to_owned();
    thread::spawn(move || drop(tx.send(UnixStream::connect(probe_path))));
    let probe = rx.recv_timeout(PROBE_TIMEOUT).map_err(|_| {
        let msg = format!(
            "probe {}: timed out; is another process busy?",
            path.display()
        );
        io::Error::new(io::ErrorKind::TimedOut, msg)
    })?;
    match probe {
        Ok(_) => {
            let msg = format!("{} is in use by another process", path.display());
            Err(io::Error::new(io::ErrorKind::AddrInUse, msg))
        }
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path),
        Err(e) => Err(e),
    }
}

fn reap(pid: u32, control: &mpsc::Sender<Control>) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    match waitpid(Pid::from_raw(raw), None) {
        Ok(status) => {
            if let Some((pid, status)) = supervisor::exit_status(status) {
                drop(control.send(Control::ChildExited { pid, status }));
            }
        }
        Err(e) => eprintln!("supervise: waitpid {pid}: {e}"),
    }
}

/// Plays Firecracker's part for host-initiated vsock connections: the host
/// connects to `<uds_path>` and sends `CONNECT <port>\n`, and gets
/// `OK <host_port>\n` back before speaking frames.
#[derive(Debug)]
struct FirecrackerMux(UnixListener);

impl ControlListener for FirecrackerMux {
    fn accept(&self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        loop {
            let (mut stream, _) = self.0.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            match read_line(&mut stream) {
                Ok(line) if line == format!("CONNECT {CONTROL_PORT}") => {
                    // Firecracker answers with the host-side port it assigned.
                    stream.write_all(b"OK 1073741824\n")?;
                    stream.set_read_timeout(None)?;
                    return Ok((Box::new(stream.try_clone()?), Box::new(stream)));
                }
                // Firecracker closes connections for ports nobody listens on.
                Ok(line) => eprintln!("supervise: refusing handshake {line:?}"),
                Err(e) => eprintln!("supervise: handshake: {e}"),
            }
        }
    }
}

/// Reads one `\n`-terminated line byte by byte, so nothing after it is
/// consumed before the frame reader takes over.
fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    while line.len() < 64 {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return String::from_utf8(line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        line.push(byte[0]);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "handshake line too long",
    ))
}
