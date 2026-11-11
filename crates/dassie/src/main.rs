//! Dassie host daemon and CLI.
//!
//! One binary: `dassie daemon` runs the reconcile loop; other subcommands are
//! clients that talk to the daemon over its Unix socket. Owns SQLite (desired
//! state + operations journal), systemd units and D-Bus, and Caddy config.
//! Composes `dassie-vm`, `dassie-image` and `dassie-proto` into app-level
//! start / clone / stop.

fn main() {}
