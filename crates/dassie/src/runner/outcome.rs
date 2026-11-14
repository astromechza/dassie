//! The pure part of vm-run: what each guest event is reported as, and what
//! vm-run exits with (ADR 0012).

use std::process;

use dassie_proto::{Event, ExitStatus};

/// Exit status when Firecracker exited cleanly but no `exited` event arrived.
pub(crate) const NO_EXITED_EVENT: u8 = 196;

/// Exit status when the VM could not run: bad config, a live socket in the
/// way, a spawn error, or Firecracker failing.
pub(crate) const VM_FAILED: u8 = 197;

/// A journal entry: field names and values, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) fields: Vec<(&'static str, String)>,
}

/// `sd_notify` message for the `ready` event.
pub(crate) const READY: &str = "READY=1\nSTATUS=ready";

/// The journal entry for a guest event, or `None` for an event vm-run does
/// not report.
pub(crate) fn entry(event: &Event) -> Option<Entry> {
    let (message, priority, extra): (String, u8, Vec<(&'static str, String)>) = match event {
        Event::Started { pid } => (
            format!("entrypoint started, pid {pid}"),
            6,
            vec![
                ("DASSIE_EVENT", "started".into()),
                ("DASSIE_PID", pid.to_string()),
            ],
        ),
        Event::Ready => (
            "entrypoint ready".into(),
            6,
            vec![("DASSIE_EVENT", "ready".into())],
        ),
        Event::Exited { status } => {
            let (message, priority, field) = match *status {
                ExitStatus::Code(code) => (
                    format!("entrypoint exited with code {code}"),
                    if code == 0 { 6 } else { 4 },
                    ("DASSIE_CODE", code.to_string()),
                ),
                ExitStatus::Signal(signal) => (
                    format!("entrypoint killed by signal {signal}"),
                    4,
                    ("DASSIE_SIGNAL", signal.to_string()),
                ),
            };
            (
                message,
                priority,
                vec![("DASSIE_EVENT", "exited".into()), field],
            )
        }
        _ => return None,
    };
    let mut fields = vec![
        ("MESSAGE", message),
        ("PRIORITY", priority.to_string()),
        ("SYSLOG_IDENTIFIER", "dassie-vm-run".into()),
    ];
    fields.extend(extra);
    Some(Entry { fields })
}

/// What vm-run exits with, plus the reason when it is not the app's own
/// status.
///
/// - `stopped`: vm-run was asked to stop (SIGTERM or SIGINT).
/// - `exited`: the status from the guest's `exited` event, if one arrived.
/// - `firecracker`: how Firecracker exited.
pub(crate) fn exit_code(
    stopped: bool,
    exited: Option<ExitStatus>,
    firecracker: process::ExitStatus,
) -> (u8, Option<String>) {
    if stopped {
        return (0, None);
    }
    if !firecracker.success() {
        return (
            VM_FAILED,
            Some(format!("firecracker failed: {firecracker}")),
        );
    }
    let code = match exited {
        None => {
            return (
                NO_EXITED_EVENT,
                Some("firecracker exited with no exited event from the guest".into()),
            );
        }
        Some(ExitStatus::Code(code)) => u8::try_from(code).ok(),
        Some(ExitStatus::Signal(signal)) => u8::try_from(signal)
            .ok()
            .filter(|s| *s < 128)
            .map(|s| 128 + s),
    };
    match code {
        Some(code) => (code, None),
        None => (
            VM_FAILED,
            Some(format!(
                "guest reported an impossible exit status {exited:?}"
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn exited_with(code: i32) -> process::ExitStatus {
        process::ExitStatus::from_raw(code << 8)
    }

    fn killed_by(signal: i32) -> process::ExitStatus {
        process::ExitStatus::from_raw(signal)
    }

    #[test]
    fn exit_codes() {
        let ok = exited_with(0);
        let cases = [
            // A requested stop is a success, however the VM went down.
            (true, Some(ExitStatus::Signal(15)), ok, 0),
            (true, None, killed_by(9), 0),
            // The app's own status, once Firecracker exited cleanly.
            (false, Some(ExitStatus::Code(0)), ok, 0),
            (false, Some(ExitStatus::Code(3)), ok, 3),
            (false, Some(ExitStatus::Code(255)), ok, 255),
            (false, Some(ExitStatus::Signal(6)), ok, 134),
            (false, Some(ExitStatus::Signal(9)), ok, 137),
            // The reserved codes.
            (false, None, ok, NO_EXITED_EVENT),
            (false, Some(ExitStatus::Code(3)), exited_with(1), VM_FAILED),
            (false, None, killed_by(9), VM_FAILED),
            (false, Some(ExitStatus::Code(256)), ok, VM_FAILED),
            (false, Some(ExitStatus::Code(-1)), ok, VM_FAILED),
            (false, Some(ExitStatus::Signal(200)), ok, VM_FAILED),
        ];
        for (stopped, exited, firecracker, want) in cases {
            let (code, reason) = exit_code(stopped, exited, firecracker);
            assert_eq!(
                code, want,
                "stopped={stopped} exited={exited:?} firecracker={firecracker}"
            );
            assert_eq!(
                reason.is_some(),
                [NO_EXITED_EVENT, VM_FAILED].contains(&want),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn entries_carry_the_event_fields() {
        let field = |event: &Event, name: &str| {
            entry(event)
                .unwrap()
                .fields
                .into_iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v)
        };
        let started = Event::Started { pid: 42 };
        assert_eq!(field(&started, "DASSIE_EVENT").as_deref(), Some("started"));
        assert_eq!(field(&started, "DASSIE_PID").as_deref(), Some("42"));
        assert_eq!(
            field(&Event::Ready, "DASSIE_EVENT").as_deref(),
            Some("ready")
        );

        let crashed = Event::Exited {
            status: ExitStatus::Signal(6),
        };
        assert_eq!(field(&crashed, "DASSIE_SIGNAL").as_deref(), Some("6"));
        assert_eq!(field(&crashed, "PRIORITY").as_deref(), Some("4"));
        let clean = Event::Exited {
            status: ExitStatus::Code(0),
        };
        assert_eq!(field(&clean, "DASSIE_CODE").as_deref(), Some("0"));
        assert_eq!(field(&clean, "PRIORITY").as_deref(), Some("6"));
    }
}
