//! An in-process fake registry for tests: a `TcpListener` and just enough
//! HTTP/1.1 to serve GETs, one connection at a time, `Connection: close`.
//!
//! It behaves like Docker Hub where the client cares:
//! - `/v2/…` answers 401 with a bearer challenge until a request carries the
//!   token from `/token`;
//! - blobs redirect (307) to `/storage/<hex>`, which refuses any request
//!   carrying an `Authorization` header, like a presigned CDN URL.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::Digest;

const TOKEN: &str = "t0ken";

#[derive(Default)]
pub(crate) struct State {
    /// Manifests by tag or digest: (content type, bytes).
    pub(crate) manifests: HashMap<String, (String, Vec<u8>)>,
    /// Blobs by hex digest.
    pub(crate) blobs: HashMap<String, Vec<u8>>,
    /// Blobs (hex) whose body is cut short after half its length.
    pub(crate) truncate: HashSet<String>,
    /// Every request: `<path> [auth]`.
    pub(crate) log: Vec<String>,
}

pub(crate) struct Fake {
    /// `127.0.0.1:<port>`: the registry part of a reference.
    pub(crate) addr: String,
    pub(crate) state: Arc<Mutex<State>>,
}

impl Fake {
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State::default()));
        let (st, a) = (state.clone(), addr.clone());
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                serve(&conn, &a, &st);
            }
        });
        Self { addr, state }
    }

    pub(crate) fn manifest(&self, key: &str, content_type: &str, bytes: &[u8]) {
        let mut st = self.state.lock().unwrap();
        st.manifests
            .insert(key.to_owned(), (content_type.to_owned(), bytes.to_vec()));
    }

    pub(crate) fn blob(&self, bytes: &[u8]) -> Digest {
        let d = Digest::of(bytes);
        let mut st = self.state.lock().unwrap();
        st.blobs.insert(d.hex().to_owned(), bytes.to_vec());
        d
    }

    /// Requests whose path starts with `prefix`.
    pub(crate) fn count(&self, prefix: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.log.iter().filter(|l| l.starts_with(prefix)).count()
    }
}

fn serve(conn: &TcpStream, addr: &str, state: &Mutex<State>) {
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
    let mut auth = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("authorization")
        {
            auth = Some(v.trim().to_owned());
        }
    }

    let mut st = state.lock().unwrap();
    st.log.push(format!(
        "{path}{}",
        if auth.is_some() { " [auth]" } else { "" }
    ));
    let challenge = format!(
        "WWW-Authenticate: Bearer realm=\"http://{addr}/token\",service=\"fake\",scope=\"repository:team/app:pull\""
    );
    let (status, headers, body, cut) = if path.starts_with("/token?") {
        let ok = path.contains("scope=repository") && path.contains("service=fake");
        if ok {
            (
                200,
                String::new(),
                format!(r#"{{"token":"{TOKEN}"}}"#).into_bytes(),
                false,
            )
        } else {
            (400, String::new(), b"bad token request".to_vec(), false)
        }
    } else if let Some(rest) = path.strip_prefix("/v2/team/app/") {
        if auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
            (401, challenge, b"{}".to_vec(), false)
        } else if let Some(key) = rest.strip_prefix("manifests/") {
            match st.manifests.get(key) {
                Some((ct, bytes)) => (200, format!("Content-Type: {ct}"), bytes.clone(), false),
                None => (
                    404,
                    String::new(),
                    br#"{"errors":[{"code":"MANIFEST_UNKNOWN"}]}"#.to_vec(),
                    false,
                ),
            }
        } else if let Some(d) = rest.strip_prefix("blobs/sha256:") {
            (
                307,
                format!("Location: http://{addr}/storage/{d}"),
                Vec::new(),
                false,
            )
        } else {
            (404, String::new(), Vec::new(), false)
        }
    } else if let Some(hex) = path.strip_prefix("/storage/") {
        match (auth, st.blobs.get(hex)) {
            (Some(_), _) => (
                400,
                String::new(),
                b"unexpected Authorization".to_vec(),
                false,
            ),
            (None, Some(b)) => (200, String::new(), b.clone(), st.truncate.contains(hex)),
            (None, None) => (404, String::new(), Vec::new(), false),
        }
    } else {
        (404, String::new(), Vec::new(), false)
    };
    drop(st);

    let mut out = conn;
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !headers.is_empty() {
        head.push_str(&headers);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let sent = if cut {
        &body[..body.len() / 2]
    } else {
        &body[..]
    };
    drop(
        out.write_all(head.as_bytes())
            .and_then(|()| out.write_all(sent)),
    );
}
