//! Where vm-run's event entries go: journald's native protocol, or stderr.

use std::env;
use std::fmt::Write as _;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use super::outcome::Entry;

/// journald's native protocol socket.
const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// Where entries are written.
#[derive(Debug, Clone)]
pub(crate) enum Journal {
    /// Datagrams in journald's native protocol, to this socket.
    Native(PathBuf),
    /// One logfmt line per entry on stderr.
    Stderr,
}

impl Journal {
    /// The journal when systemd connected stderr to it (`$JOURNAL_STREAM`),
    /// stderr otherwise.
    pub(crate) fn from_env() -> Self {
        if env::var_os("JOURNAL_STREAM").is_some() {
            Self::Native(JOURNAL_SOCKET.into())
        } else {
            Self::Stderr
        }
    }

    /// Writes one entry. A failed journal send falls back to stderr.
    pub(crate) fn send(&self, entry: &Entry) {
        if let Self::Native(socket) = self {
            let sent = UnixDatagram::unbound().and_then(|s| s.send_to(&native(entry), socket));
            match sent {
                Ok(_) => return,
                Err(e) => eprintln!("vm-run: journal {}: {e}", socket.display()),
            }
        }
        eprintln!("{}", logfmt(entry));
    }
}

/// An entry in journald's native protocol: one `KEY=VALUE\n` per field.
/// vm-run's values never hold a newline; one that did would be read as a
/// new field, so newlines are replaced with spaces.
pub(crate) fn native(entry: &Entry) -> Vec<u8> {
    let mut out = String::new();
    for (key, value) in &entry.fields {
        let _ = writeln!(out, "{key}={}", value.replace('\n', " "));
    }
    out.into_bytes()
}

/// An entry as one stderr line, such as `vm-run: event=exited code=3`: the
/// `DASSIE_*` fields, lower-cased and without the prefix.
pub(crate) fn logfmt(entry: &Entry) -> String {
    let mut out = String::from("vm-run:");
    for (key, value) in &entry.fields {
        if let Some(name) = key.strip_prefix("DASSIE_") {
            let _ = write!(out, " {}={value}", name.to_ascii_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Entry {
        Entry {
            fields: vec![
                ("MESSAGE", "two\nlines".into()),
                ("DASSIE_EVENT", "exited".into()),
                ("DASSIE_CODE", "3".into()),
            ],
        }
    }

    #[test]
    fn encodes_native_fields_one_per_line() {
        assert_eq!(
            String::from_utf8(native(&sample())).unwrap(),
            "MESSAGE=two lines\nDASSIE_EVENT=exited\nDASSIE_CODE=3\n"
        );
    }

    #[test]
    fn logfmt_keeps_only_dassie_fields() {
        assert_eq!(logfmt(&sample()), "vm-run: event=exited code=3");
    }
}
