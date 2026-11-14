//! `sd_notify` over `$NOTIFY_SOCKET`, std only.

use std::env;
use std::path::PathBuf;

use super::send_datagram;

/// The service manager's notification socket, if there is one to use.
#[derive(Debug, Clone, Default)]
pub(crate) struct Notify(Option<PathBuf>);

impl Notify {
    #[cfg(test)]
    pub(crate) fn new(socket: Option<PathBuf>) -> Self {
        Self(socket)
    }

    /// `$NOTIFY_SOCKET`, when it is a path. systemd's system manager uses
    /// `/run/systemd/notify`. Abstract (`@…`) sockets are not supported and
    /// are skipped with a warning.
    pub(crate) fn from_env() -> Self {
        match env::var_os("NOTIFY_SOCKET") {
            None => Self(None),
            Some(s) if s.as_encoded_bytes().starts_with(b"@") => {
                eprintln!(
                    "vm-run: abstract NOTIFY_SOCKET {} is not supported; not notifying",
                    s.display()
                );
                Self(None)
            }
            Some(s) => Self(Some(s.into())),
        }
    }

    /// Sends one notification, such as `READY=1`. A failure is logged and
    /// otherwise ignored: systemd then times the start out.
    pub(crate) fn send(&self, message: &str) {
        let Some(socket) = &self.0 else {
            return;
        };
        if let Err(e) = send_datagram(socket, message.as_bytes()) {
            eprintln!("vm-run: notify {}: {e}", socket.display());
        }
    }
}
