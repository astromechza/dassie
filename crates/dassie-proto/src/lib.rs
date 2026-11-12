//! Dassie host <-> guest protocol.
//!
//! Message types and framing for the vsock channels between `dassie-init` and
//! the host. Must build for the static musl guest init, so the dependency
//! budget is serde plus `serde_json` and no async runtime. Framing is generic
//! so transports can be swapped in tests: [`FrameWriter`] takes any `Write`,
//! [`FrameReader`] any `BufRead` (wrap streams such as `UnixStream` in a
//! `BufReader`). The wire format is specified in ADR 0004.
//!
//! Two channels, both carried over Firecracker's vsock device:
//!
//! - **events** ([`EVENTS_PORT`]): guest -> host [`Event`]s. The guest
//!   connects to the host (CID 2); Firecracker forwards the connection to the
//!   host Unix socket `<uds_path>_1024`.
//! - **control** ([`CONTROL_PORT`]): host -> guest [`Request`]s, each answered
//!   by one [`Response`]. The host connects to `<uds_path>`, sends
//!   `CONNECT 1025\n` and reads `OK <port>\n` before speaking frames.

// Parses bytes from the guest, which the host must treat as untrusted: no
// panicking paths and no silent truncating casts.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

pub mod frame;
pub mod message;

pub use frame::{Error, FrameReader, FrameWriter};
pub use message::{
    ErrorCode, Event, ExitStatus, GuestState, HealthStatus, Message, Request, Response,
};

/// Protocol version carried in every frame's `v` field.
pub const PROTOCOL_VERSION: u32 = 1;

/// vsock port for guest -> host events.
pub const EVENTS_PORT: u32 = 1024;

/// vsock port for host -> guest control requests.
pub const CONTROL_PORT: u32 = 1025;

/// Maximum frame length in bytes, including the `\n` terminator.
pub const MAX_LINE: usize = 65536;
