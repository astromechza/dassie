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

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use std::{env, fs, io, thread};

use dassie_proto::{
    CONTROL_PORT, EVENTS_PORT, Error, Event, FrameReader, FrameWriter, Request, Response,
};

/// Concurrent event connections served; a guest normally holds one.
const MAX_CONNECTIONS: usize = 8;

const USAGE: &str = "usage:
  listener events <uds_path> [--until exited] [--timeout SECS]
  listener health <uds_path>
  listener stop   <uds_path> [--timeout-ms N]";

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("listener: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<ExitCode, String> {
    let (cmd, uds, flags) = match args.as_slice() {
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
    let path = format!("{uds}_{EVENTS_PORT}");
    remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path).map_err(|e| format!("bind {path}: {e}"))?;
    eprintln!("listening on {path}");

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

/// Removes a socket left behind by a previous run. Refuses a path that is not
/// a socket or that another listener is still serving.
fn remove_stale_socket(path: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("stat {path}: {e}")),
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(format!("{path} exists and is not a socket"));
        }
        Ok(_) => {}
    }
    if UnixStream::connect(path).is_ok() {
        return Err(format!("{path} is in use by another listener"));
    }
    fs::remove_file(path).map_err(|e| format!("remove stale {path}: {e}"))
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
            Err(e @ (Error::Version(_) | Error::UnknownType(_) | Error::Invalid(_))) => {
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
    let stream = UnixStream::connect(uds).map_err(|e| format!("connect {uds}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;

    // Firecracker's host-initiated handshake: `CONNECT <port>\n` -> `OK <n>\n`.
    writeln!(writer, "CONNECT {CONTROL_PORT}").map_err(|e| format!("handshake: {e}"))?;
    let mut ack = String::new();
    reader
        .read_line(&mut ack)
        .map_err(|e| format!("handshake: {e}"))?;
    if !ack.starts_with("OK ") {
        return Err(format!(
            "handshake refused ({ack:?}); is the guest listening on port {CONTROL_PORT}?"
        ));
    }

    FrameWriter::new(writer)
        .send(request)
        .map_err(|e| format!("send: {e}"))?;
    match FrameReader::new(reader).recv::<Response>() {
        Ok(Some(response @ Response::Health(_))) => {
            println!("{response:?}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(Some(response)) => {
            println!("{response:?}");
            Ok(ExitCode::FAILURE)
        }
        Ok(None) => Err("guest closed the connection without a response".into()),
        Err(e) => Err(format!("receive: {e}")),
    }
}
