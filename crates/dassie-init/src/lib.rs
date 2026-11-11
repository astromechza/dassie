//! Portable parts of the dassie guest init (ADR 0005).
//!
//! Config loading and the entrypoint supervisor live here, free of PID-1
//! syscalls (mount, chroot, reboot), so they build and run as an ordinary
//! process on any Unix and are unit-tested on a macOS dev machine (ADR 0003).
//! The Linux-only PID-1 layer in `main.rs` composes them.

pub mod config;
pub mod control;
pub mod reporter;
pub mod supervisor;
pub mod user;
