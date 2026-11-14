//! Host-side stand-in for the dassie daemon during M0.
//!
//! ```text
//! listener events <uds_path> [--until exited] [--timeout SECS]
//! listener health <uds_path>
//! listener stop   <uds_path> [--timeout-ms N]
//! ```
//!
//! `<uds_path>` is the Firecracker vsock device's `uds_path`. `events` listens
//! on `<uds_path>_1024` for guest-initiated connections and logs every event.
//! `health` and `stop` connect to `<uds_path>`, do Firecracker's
//! `CONNECT 1025` handshake, send one control request and print the response.
//! Both use [`dassie_vm::vsock`].

// A CLI: stdout is its output.
#![allow(clippy::print_stdout)]

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use std::{env, thread};

use dassie_proto::{EVENTS_PORT, Error, Event, FrameReader, Request, Response};
use dassie_vm::{ControlClient, guest_port_path, remove_stale_socket};

/// Concurrent event connections served; a guest normally holds one.
const MAX_CONNECTIONS: usize = 8;

/// Bounds each read and write of a control request.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "usage:
  listener events <uds_path> [--until exited] [--timeout SECS]
  listener health <uds_path>
  listener stop   <uds_path> [--timeout-ms N]";

fn main() -> ExitCode {
    match run(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("listener: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let (cmd, uds, flags) = match args {
        [cmd, uds, flags @ ..] => (cmd.as_str(), uds.as_str(), flags),
        _ => return Err(USAGE.into()),
    };
    match cmd {
        "events" => {
            let mut until_exited = false;
            let mut timeout = None;
            let mut it = flags.iter();
            while let Some(flag) = it.next() {
                match (flag.as_str(), it.next().map(String::as_str)) {
                    ("--until", Some("exited")) => until_exited = true,
                    ("--timeout", Some(secs)) => {
                        let secs = secs.parse().map_err(|_| format!("bad --timeout {secs}"))?;
                        timeout = Some(Duration::from_secs(secs));
                    }
                    _ => return Err(USAGE.into()),
                }
            }
            events(uds, until_exited, timeout)
        }
        "health" if flags.is_empty() => control(uds, &Request::Health),
        "stop" => {
            let timeout_ms = match flags {
                [] => None,
                [flag, ms] if flag == "--timeout-ms" => {
                    Some(ms.parse().map_err(|_| format!("bad --timeout-ms {ms}"))?)
                }
                _ => return Err(USAGE.into()),
            };
            control(uds, &Request::Stop { timeout_ms })
        }
        _ => Err(USAGE.into()),
    }
}

/// Logs guest events. With `until_exited`, returns success after the first
/// `exited` event, or failure once `timeout` elapses.
fn events(uds: &str, until_exited: bool, timeout: Option<Duration>) -> Result<ExitCode, String> {
    let path = guest_port_path(Path::new(uds), EVENTS_PORT);
    remove_stale_socket(&path).map_err(|e| e.to_string())?;
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    eprintln!("listening on {}", path.display());

    let (exited_tx, exited_rx) = mpsc::channel();
    let active = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for (n, conn) in listener.incoming().enumerate() {
            match conn {
                // A guest uses one connection at a time; refuse a flood
                // before it costs a thread.
                Ok(_) if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS => {
                    eprintln!("[conn {n}] refused: {MAX_CONNECTIONS} connections already open");
                }
                Ok(conn) => {
                    active.fetch_add(1, Ordering::SeqCst);
                    let (exited_tx, active) = (exited_tx.clone(), Arc::clone(&active));
                    thread::spawn(move || {
                        log_events(n, conn, &exited_tx);
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) => eprintln!("accept: {e}"),
            }
        }
    });

    let outcome = match (until_exited, timeout) {
        (false, None) => loop {
            thread::park();
        },
        (false, Some(t)) => {
            thread::sleep(t);
            return Ok(ExitCode::SUCCESS);
        }
        (true, None) => exited_rx.recv().map_err(|e| e.to_string()),
        (true, Some(t)) => exited_rx.recv_timeout(t).map_err(|e| e.to_string()),
    };
    match outcome {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) => Err(format!("no exited event: {e}")),
    }
}

fn log_events(conn_id: usize, conn: UnixStream, exited_tx: &mpsc::Sender<()>) {
    eprintln!("[conn {conn_id}] connected");
    let mut reader = FrameReader::new(BufReader::new(conn));
    loop {
        match reader.recv::<Event>() {
            Ok(Some(event)) => {
                println!("[conn {conn_id}] {event:?}");
                if matches!(event, Event::Exited { .. }) {
                    let _ = exited_tx.send(());
                }
            }
            Ok(None) => break,
            // Skippable: log and read the next frame.
            Err(e @ (Error::UnsupportedVersion(_) | Error::UnknownType(_) | Error::Invalid(_))) => {
                eprintln!("[conn {conn_id}] skipping frame: {e}");
            }
            Err(e) => {
                eprintln!("[conn {conn_id}] {e}");
                break;
            }
        }
    }
    eprintln!("[conn {conn_id}] closed");
}

/// Sends one control request through the Firecracker vsock mux and prints the
/// response.
fn control(uds: &str, request: &Request) -> Result<ExitCode, String> {
    let mut client =
        ControlClient::connect(Path::new(uds), CONTROL_TIMEOUT).map_err(|e| e.to_string())?;
    match client.request(request).map_err(|e| e.to_string())? {
        response @ Response::Health(_) => {
            println!("{response:?}");
            Ok(ExitCode::SUCCESS)
        }
        response => {
            println!("{response:?}");
            Ok(ExitCode::FAILURE)
        }
    }
}
