//! Where vm-run's event entries go: journald's native protocol, or stderr.

use std::env;
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use super::outcome::Entry;
use super::send_datagram;

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
    /// The journal when systemd connected stderr to it, stderr otherwise.
    ///
    /// `$JOURNAL_STREAM` is inherited, so a process whose stderr was
    /// redirected (a CI runner that is itself a systemd service, a shell
    /// with `2>file`) still has it. As `systemd.exec(5)` says, it counts only
    /// when its `<device>:<inode>` is stderr's.
    pub(crate) fn from_env() -> Self {
        let to_journal = env::var("JOURNAL_STREAM")
            .ok()
            .zip(stderr_id().ok())
            .is_some_and(|(var, (dev, ino))| is_stream(&var, dev, ino));
        if to_journal {
            Self::Native(JOURNAL_SOCKET.into())
        } else {
            Self::Stderr
        }
    }

    /// Writes one entry. A failed journal send falls back to stderr.
    pub(crate) fn send(&self, entry: &Entry) {
        if let Self::Native(socket) = self {
            match send_datagram(socket, &native(entry)) {
                Ok(()) => return,
                Err(e) => eprintln!("vm-run: journal {}: {e}", socket.display()),
            }
        }
        eprintln!("{}", logfmt(entry));
    }
}

/// The device and inode of this process's stderr.
fn stderr_id() -> io::Result<(u64, u64)> {
    let meta = File::from(io::stderr().as_fd().try_clone_to_owned()?).metadata()?;
    Ok((meta.dev(), meta.ino()))
}

/// Whether `$JOURNAL_STREAM` (`<device>:<inode>`, in decimal) names this
/// device and inode.
fn is_stream(var: &str, dev: u64, ino: u64) -> bool {
    var.split_once(':')
        .and_then(|(d, i)| Some((d.parse::<u64>().ok()?, i.parse::<u64>().ok()?)))
        == Some((dev, ino))
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

    #[test]
    fn journal_stream_must_name_stderr() {
        assert!(is_stream("8:12345", 8, 12345));
        assert!(!is_stream("8:12345", 8, 99), "another stream");
        assert!(!is_stream("8:12345", 9, 12345), "another device");
        for bad in ["", "8", "8:", ":1", "x:1", "8:1:2"] {
            assert!(!is_stream(bad, 8, 1), "{bad:?}");
        }
    }

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
