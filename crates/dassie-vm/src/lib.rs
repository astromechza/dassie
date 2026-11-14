//! Firecracker-facing VM operations.
//!
//! Reads Firecracker configuration and speaks the host side of Firecracker's
//! vsock device: the `CONNECT` handshake for host-initiated connections and
//! the `<uds_path>_<port>` sockets for guest-initiated ones (ADR 0004), and
//! builds the jailer's chroot (ADR 0013). Later it renders Firecracker
//! configuration and drives the API socket (pause, snapshot, restore). Knows nothing about systemd: the `dassie` crate
//! owns units and decides when VMs start and stop.

pub mod config;
pub mod jail;
pub mod vsock;

pub use config::vsock_uds_path;
pub use jail::Jail;
pub use vsock::{ControlClient, ControlError, guest_port_path, remove_stale_socket};
