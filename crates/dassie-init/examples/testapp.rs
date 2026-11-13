//! A trivial entrypoint for the test rootfs (`guest/rootfs.sh`): answers every
//! HTTP request with `200 OK`. std only, so it builds as a static musl binary.
//!
//! - `PORT`: TCP port to listen on, on all addresses (default 8080).
//! - `EXIT_AFTER`: if set, exit with code 0 after this many seconds, so a
//!   boot test sees `started`, `ready` and `exited` without a stop request.
//! - `GET /crash` aborts the process (SIGABRT), for the "kill the
//!   entrypoint" drill in `guest/systemd/README.md`.
//! - `GET /dial?addr=HOST:PORT` opens a TCP connection from inside the guest,
//!   resolving `HOST` through `/etc/resolv.conf`, and answers `200 ok ADDR`
//!   or `502 error: …`. The network drills use it for isolation and egress.
//!
//! At startup it reopens its stderr through `/dev/stderr` and writes to
//! `/dev/shm`, and exits if either fails. It runs as a non-root user, so the
//! boot test also checks that init gives such apps the `/dev` that OCI
//! runtimes do.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, ToSocketAddrs};
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

    check_dev()?;
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

fn env_or(key: &str, default: u16) -> Result<u16, String> {
    env::var(key).map_or(Ok(default), |v| {
        v.parse().map_err(|e| format!("bad {key}={v:?}: {e}"))
    })
}

/// How long `/dial` waits for each resolved address to connect.
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Reads the request head, then answers 200, or aborts on `GET /crash`, or
/// dials out on `GET /dial`.
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
        let mut last = std::io::Error::other("no addresses");
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
    eprintln!("testapp: dial {addr}: {}", body.trim_end());
    (status, body)
}
