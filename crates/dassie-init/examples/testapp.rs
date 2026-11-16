//! A trivial entrypoint for the test rootfs (`scripts/guest/rootfs.sh`):
//! answers every HTTP request with `200 OK`. std only, so it builds as a static
//! musl binary.
//!
//! - `PORT`: TCP port to listen on, on all addresses (default 8080).
//! - `EXIT_AFTER`: if set, exit after this many seconds, so a boot test sees
//!   `started`, `ready` and `exited` without a stop request.
//! - `EXIT_CODE`: the code `EXIT_AFTER` exits with (default 0), so a boot test
//!   can check that it reaches the host as `dassie vm-run`'s exit status.
//! - `CRASH_AFTER`: if set, abort (SIGABRT) after this many seconds, for the
//!   crash drills in `scripts/host/systemd/README.md` on a VM with no NIC.
//! - `GET /crash` aborts the process (SIGABRT), for the "kill the
//!   entrypoint" drill in `scripts/host/systemd/README.md`.
//! - `GET /dial?addr=HOST:PORT` opens a TCP connection from inside the guest,
//!   resolving `HOST` through `/etc/resolv.conf`, and answers `200 ok ADDR`
//!   or `502 error: …`. The network drills use it for isolation and egress.
//!
//! At startup it logs its argv, `GREETING`, `DASSIE_INSTANCE_ID`, its
//! hostname with the address it resolves to, and the source address of its
//! default route, so the boot test can check the guest drive's config
//! reached it (ADR 0015).
//!
//! It then reopens its stderr through `/dev/stderr` and writes to
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
use std::net::{Ipv4Addr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;
use std::{env, io, process, thread};

/// Logs one `testapp: ` line with a single `write`. `eprintln!` writes a
/// line in pieces, so a line PID 1 prints at the same moment could land in
/// the middle of it on the shared console, and the boot test's grep miss.
macro_rules! say {
    ($($arg:tt)*) => {
        drop(io::stderr().write_all(format!("testapp: {}\n", format_args!($($arg)*)).as_bytes()))
    };
}

fn main() -> Result<(), Box<dyn Error>> {
    let port: u16 = env_or("PORT", 8080)?;
    if let Ok(secs) = env::var("EXIT_AFTER") {
        let secs: u64 = secs.parse().map_err(|e| format!("bad EXIT_AFTER: {e}"))?;
        let code: i32 = env::var("EXIT_CODE").map_or(Ok(0), |c| {
            c.parse().map_err(|e| format!("bad EXIT_CODE={c:?}: {e}"))
        })?;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            say!("EXIT_AFTER={secs} elapsed, exiting {code}");
            process::exit(code);
        });
    }
    if let Ok(secs) = env::var("CRASH_AFTER") {
        let secs: u64 = secs.parse().map_err(|e| format!("bad CRASH_AFTER: {e}"))?;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            say!("CRASH_AFTER={secs} elapsed, aborting");
            process::abort();
        });
    }

    report_config();
    check_dev()?;
    check_root()?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
    say!("listening on port {port}");
    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                thread::spawn(move || {
                    if let Err(e) = respond(&conn) {
                        say!("{e}");
                    }
                });
            }
            Err(e) => say!("accept: {e}"),
        }
    }
    Ok(())
}

/// Logs what the guest config sets, for the boot test to match.
fn report_config() {
    let args: Vec<String> = env::args().collect();
    say!("argv {args:?}");
    for key in ["GREETING", "DASSIE_INSTANCE_ID"] {
        say!("{key}={:?}", env::var(key).unwrap_or_default());
    }
    let hostname = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let hostname = hostname.trim_end();
    match (hostname, 0).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(sa) => say!("hostname {hostname} -> {}", sa.ip()),
            None => say!("hostname {hostname} has no address"),
        },
        Err(e) => say!("hostname {hostname}: {e}"),
    }
    // A UDP connect picks the route and source address without sending
    // anything; with no default route it fails.
    let route = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|sock| {
        sock.connect((Ipv4Addr::new(1, 1, 1, 1), 53))?;
        sock.local_addr()
    });
    match route {
        Ok(addr) => say!("default route from {}", addr.ip()),
        Err(e) => say!("no default route: {e}"),
    }
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
    say!("/dev/shm ok");
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
    say!("/ is 0755 root");
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
    say!("clean root, write ok");
    Ok(())
}

/// Written on the rootfs at every start (the overlay's upper layer).
const BOOT_MARKER: &str = "/tmp/testapp.boot";

fn env_or(key: &str, default: u16) -> Result<u16, String> {
    env::var(key).map_or(Ok(default), |v| {
        v.parse().map_err(|e| format!("bad {key}={v:?}: {e}"))
    })
}

/// How long `/dial` waits for each resolved address to connect.
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Reads the request head, then answers 200, or aborts on `GET /crash`, or
/// dials out on `GET /dial`.
fn respond(mut conn: &TcpStream) -> io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    // A client that closes without sending a request, like the readiness
    // probe, gets no response.
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    if line.starts_with("GET /crash ") {
        say!("/crash requested, aborting");
        process::abort();
    }
    let target = line.split(' ').nth(1).unwrap_or_default().to_owned();
    let mut head = line.clone();
    while !matches!(head.as_str(), "" | "\r\n" | "\n") {
        head.clear();
        reader.read_line(&mut head)?;
    }
    let (status, body) = match target.strip_prefix("/dial") {
        Some(query) => dial(query),
        None => ("200 OK", "ok\n".to_owned()),
    };
    write!(
        conn,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Handles `/dial?addr=HOST:PORT`: tries each resolved address in turn.
fn dial(query: &str) -> (&'static str, String) {
    let Some(addr) = query
        .strip_prefix('?')
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("addr="))
        .filter(|a| !a.is_empty())
    else {
        return (
            "400 Bad Request",
            "error: want /dial?addr=HOST:PORT\n".to_owned(),
        );
    };
    let result = addr.to_socket_addrs().and_then(|addrs| {
        let mut last = io::Error::other("no addresses");
        for sa in addrs {
            match TcpStream::connect_timeout(&sa, DIAL_TIMEOUT) {
                Ok(_) => return Ok(sa),
                Err(e) => last = e,
            }
        }
        Err(last)
    });
    let (status, body) = match result {
        Ok(sa) => ("200 OK", format!("ok {sa}\n")),
        Err(e) => ("502 Bad Gateway", format!("error: {e}\n")),
    };
    say!("dial {addr}: {}", body.trim_end());
    (status, body)
}
