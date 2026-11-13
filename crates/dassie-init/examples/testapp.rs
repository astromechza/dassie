//! A trivial entrypoint for the test rootfs (`guest/rootfs.sh`): answers every
//! HTTP request with `200 OK`. std only, so it builds as a static musl binary.
//!
//! - `PORT`: TCP port to listen on, on all addresses (default 8080).
//! - `EXIT_AFTER`: if set, exit with code 0 after this many seconds, so a
//!   boot test sees `started`, `ready` and `exited` without a stop request.
//! - `CRASH_AFTER`: if set, abort (SIGABRT) after this many seconds, for the
//!   crash drills in `guest/systemd/README.md` on a VM with no NIC.
//! - `GET /crash` aborts the process (SIGABRT), for the "kill the
//!   entrypoint" drill in `guest/systemd/README.md`.
//!
//! At startup it reopens its stderr through `/dev/stderr` and writes to
//! `/dev/shm`, and exits if either fails. It runs as a non-root user, so the
//! boot test also checks that init gives such apps the `/dev` that OCI
//! runtimes do.
//!
//! It also checks the overlay root (ADR 0006): `/` must keep the image's
//! `0755 root`, not the tmpfs's `1777`, and a file it writes to the root
//! filesystem must not be there yet. A file left from an earlier boot of the
//! same rootfs means the overlay did not discard it, and testapp exits 1.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;
use std::{env, process, thread};

fn main() -> Result<(), Box<dyn Error>> {
    let port: u16 = env_or("PORT", 8080)?;
    if let Ok(secs) = env::var("EXIT_AFTER") {
        let secs: u64 = secs.parse().map_err(|e| format!("bad EXIT_AFTER: {e}"))?;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            eprintln!("testapp: EXIT_AFTER={secs} elapsed, exiting");
            process::exit(0);
        });
    }
    if let Ok(secs) = env::var("CRASH_AFTER") {
        let secs: u64 = secs.parse().map_err(|e| format!("bad CRASH_AFTER: {e}"))?;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            eprintln!("testapp: CRASH_AFTER={secs} elapsed, aborting");
            process::abort();
        });
    }

    check_dev()?;
    check_root()?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
    eprintln!("testapp: listening on port {port}");
    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                thread::spawn(move || {
                    if let Err(e) = respond(&conn) {
                        eprintln!("testapp: {e}");
                    }
                });
            }
            Err(e) => eprintln!("testapp: accept: {e}"),
        }
    }
    Ok(())
}

/// What an image like nginx does at startup: log through `/dev/stderr`, and
/// use POSIX shared memory.
fn check_dev() -> Result<(), String> {
    let mut stderr = OpenOptions::new()
        .append(true)
        .open("/dev/stderr")
        .map_err(|e| format!("open /dev/stderr: {e}"))?;
    writeln!(stderr, "testapp: /dev/stderr ok").map_err(|e| format!("write /dev/stderr: {e}"))?;
    let shm = format!("/dev/shm/testapp.{}", process::id());
    fs::write(&shm, b"ok").map_err(|e| format!("write {shm}: {e}"))?;
    fs::remove_file(&shm).map_err(|e| format!("remove {shm}: {e}"))?;
    eprintln!("testapp: /dev/shm ok");
    Ok(())
}

/// The overlay root keeps the image's root mode and owner, and starts every
/// boot without the previous boot's writes.
fn check_root() -> Result<(), String> {
    let root = fs::metadata("/").map_err(|e| format!("stat /: {e}"))?;
    let mode = root.mode() & 0o7777;
    if root.uid() != 0 || mode != 0o755 {
        return Err(format!("/ is {mode:o} uid {}, want 755 root", root.uid()));
    }
    eprintln!("testapp: / is 0755 root");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(BOOT_MARKER)
    {
        Ok(mut f) => {
            writeln!(f, "{}", process::id()).map_err(|e| format!("write {BOOT_MARKER}: {e}"))?;
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            return Err(format!(
                "{BOOT_MARKER} exists: stale write from a previous boot"
            ));
        }
        Err(e) => return Err(format!("create {BOOT_MARKER}: {e}")),
    }
    eprintln!("testapp: clean root, write ok");
    Ok(())
}

/// Written on the rootfs at every start (the overlay's upper layer).
const BOOT_MARKER: &str = "/tmp/testapp.boot";

fn env_or(key: &str, default: u16) -> Result<u16, String> {
    env::var(key).map_or(Ok(default), |v| {
        v.parse().map_err(|e| format!("bad {key}={v:?}: {e}"))
    })
}

/// Reads the request head, then answers 200, or aborts on `GET /crash`.
fn respond(mut conn: &TcpStream) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    // A client that closes without sending a request, like the readiness
    // probe, gets no response.
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    if line.starts_with("GET /crash ") {
        eprintln!("testapp: /crash requested, aborting");
        process::abort();
    }
    while !matches!(line.as_str(), "" | "\r\n" | "\n") {
        line.clear();
        reader.read_line(&mut line)?;
    }
    let body = "ok\n";
    write!(
        conn,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
