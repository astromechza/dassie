//! Message types for the events and control channels.
//!
//! The JSON shape of every type here is the wire contract (ADR 0004). The
//! envelope `v` field is added and checked by [`crate::frame`], not here.

use serde::{Deserialize, Serialize};

/// A message type that can be framed on a dassie channel.
pub trait Message: Serialize + for<'de> Deserialize<'de> {
    /// Every `type` tag this message enum knows. Lets a receiver tell an
    /// unknown (newer) message type apart from a malformed known one.
    const TYPES: &'static [&'static str];
}

/// Guest -> host lifecycle event, sent on the events channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// The entrypoint was spawned.
    Started {
        /// Entrypoint PID inside the guest.
        pid: u32,
    },
    /// The entrypoint is ready: its port accepted a TCP connect, or it was
    /// spawned and declares no port.
    Ready,
    /// The entrypoint exited; the VM reboots right after.
    Exited {
        #[serde(flatten)]
        status: ExitStatus,
    },
}

impl Message for Event {
    const TYPES: &'static [&'static str] = &["started", "ready", "exited"];
}

/// How the entrypoint ended. On the wire exactly one of `code` or `signal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[serde(try_from = "RawExitStatus", into = "RawExitStatus")]
pub enum ExitStatus {
    /// Exited normally with this code.
    Code(i32),
    /// Killed by this signal number.
    Signal(i32),
}

#[derive(Serialize, Deserialize)]
struct RawExitStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
}

impl TryFrom<RawExitStatus> for ExitStatus {
    type Error = &'static str;

    fn try_from(raw: RawExitStatus) -> Result<Self, Self::Error> {
        match (raw.code, raw.signal) {
            (Some(code), None) => Ok(Self::Code(code)),
            (None, Some(signal)) => Ok(Self::Signal(signal)),
            _ => Err("exactly one of `code` or `signal` is required"),
        }
    }
}

impl From<ExitStatus> for RawExitStatus {
    fn from(status: ExitStatus) -> Self {
        match status {
            ExitStatus::Code(code) => Self {
                code: Some(code),
                signal: None,
            },
            ExitStatus::Signal(signal) => Self {
                code: None,
                signal: Some(signal),
            },
        }
    }
}

/// Host -> guest request on the control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Request {
    /// Report the current lifecycle state.
    Health,
    /// Stop the entrypoint gracefully: SIGTERM, then SIGKILL after the
    /// timeout. Repeats do not restart or extend the deadline.
    Stop {
        /// Grace period before SIGKILL; the guest default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

impl Message for Request {
    const TYPES: &'static [&'static str] = &["health", "stop"];
}

/// Guest -> host response on the control channel, one per request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Response {
    /// Answer to `health` and `stop`.
    Health(HealthStatus),
    /// The guest could not serve the request.
    Error {
        code: ErrorCode,
        /// Human-readable detail.
        message: String,
    },
}

impl Message for Response {
    const TYPES: &'static [&'static str] = &["health", "error"];
}

/// Body of a `health` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct HealthStatus {
    pub state: GuestState,
    /// Entrypoint PID inside the guest.
    pub pid: u32,
}

/// Lifecycle state of the entrypoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum GuestState {
    /// Spawned, not yet ready.
    Starting,
    /// Readiness reached.
    Ready,
    /// Stop requested; SIGTERM sent.
    Stopping,
    /// Entrypoint gone; reboot imminent.
    Exited,
}

/// Why a request could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Unknown request `type` (e.g. a newer host talking to an older guest).
    Unsupported,
    /// Invalid JSON, bad fields or unsupported `v`.
    BadRequest,
}
