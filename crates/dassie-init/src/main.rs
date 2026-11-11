//! Dassie guest PID 1.
//!
//! Static musl binary running inside each microVM: sets up the guest, runs the
//! container entrypoint, reports lifecycle events to the host over vsock, and
//! reboots the VM when the entrypoint exits. Linux only; the non-Linux build
//! exists so the workspace still compiles on a macOS dev machine.

#[cfg(target_os = "linux")]
fn main() {}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("dassie-init only runs on Linux");
    std::process::exit(1);
}
