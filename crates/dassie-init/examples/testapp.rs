//! A trivial entrypoint for the test rootfs (`guest/rootfs.sh`): answers every
//! HTTP request with `200 OK`. std only, so it builds as a static musl binary.
//!
//! - `PORT`: TCP port to listen on, on all addresses (default 8080).
//! - `EXIT_AFTER`: if set, exit with code 0 after this many seconds, so a
//!   boot test sees `started`, `ready` and `exited` without a stop request.
//! - `GET /crash` aborts the process (SIGABRT), for the "kill the
//!   entrypoint" drill in `guest/systemd/README.md`.

use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
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
    reader.read_line(&mut line)?;
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
