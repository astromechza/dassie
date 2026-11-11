# 0003. Linux-only targets

Status: Accepted
Date: 2026-11-11

## Context

Firecracker needs KVM; the jailer needs Linux namespaces, cgroups and seccomp;
the daemon drives systemd over D-Bus and creates TAP devices; the guest init
uses Linux syscalls and `AF_VSOCK`. Development happens on an Apple Silicon
macOS machine, which has none of these. Production hosts are most likely
x86_64; a local Linux VM on Apple Silicon is aarch64 and can run Firecracker
only with nested virtualisation (M3 or later, macOS 15 or later). Standard
GitHub Actions Linux runners are x86_64 and expose `/dev/kvm`.

## Decision

- Dassie targets Linux only: `x86_64-unknown-linux-musl` (primary) and
  `aarch64-unknown-linux-musl` (secondary). Both are listed in
  `rust-toolchain.toml`.
- macOS is a development machine only. Nothing is expected to run there.
- Linux-only code sits behind `#[cfg(target_os = "linux")]` so that
  `cargo build` and `cargo clippy --workspace` stay green on macOS. The
  non-Linux build of `dassie-init` exits with an error.
- End-to-end and integration tests that need KVM, Firecracker, systemd or TAP
  run on an x86_64 Linux GitHub Actions runner or a local aarch64 Linux VM.

## Consequences

Cannot run on macOS: Firecracker and the jailer, systemd and D-Bus, TAP and
network namespaces, guest-side vsock, and all of `dassie-init`.

Portable: unit, Caddy and Firecracker config rendering; SQLite; the CLI socket;
the host-side vsock listener (Firecracker exposes it as a plain Unix socket,
`<uds_path>_<port>`); the Caddy admin API. `dassie-image` is only partly
portable: unpacking OCI layers onto APFS loses ownership, device nodes and
xattrs, so rootfs assembly must not go through a macOS directory.

Intended test strategy, so most logic is testable on macOS:

- Prefer protocol-level fakes over trait mocks where the real interface is a
  socket: a fake Firecracker API server on a Unix socket, a Unix socket in
  place of vsock, and real Caddy.
- `dassie-init` keeps its supervisor logic (spawn, reap, signal forwarding,
  event reporting) separate from PID-1 syscalls (mount, reboot), so the
  supervisor runs as an ordinary process in tests.
- Trait seams only where no cheap protocol exists: systemd D-Bus and
  netlink/TAP, each with an in-memory fake.
- Every path the daemon writes is under a configurable root directory, so tests
  use a temporary directory.
- Timers use paused or injected time; OCI tests use local OCI-layout fixtures.
- No process-based VM backend: real end-to-end tests always run Firecracker on
  KVM, keeping a single runtime path.

Each of these gets built, and gets its own ADR if it involves a further
decision, in the change that needs it.
