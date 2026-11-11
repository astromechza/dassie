//! Firecracker-facing VM operations.
//!
//! Renders Firecracker and jailer configuration and drives the Firecracker API
//! socket (pause, snapshot, restore). Knows nothing about systemd: the `dassie`
//! crate owns units and decides when VMs start and stop.
