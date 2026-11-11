# dassie

- macOS is the development machine; build targets are Linux only (x86_64 primary, aarch64 secondary). See README.md for build and check commands.
- Linux-only code goes behind `#[cfg(target_os = "linux")]` so `cargo build` and `cargo clippy --workspace` stay green on macOS. Keep pure logic (rendering, state machines, protocol framing) portable and unit-tested.
- Anything needing KVM, Firecracker, systemd or TAP is tested on Linux (GitHub runner or Linux VM), never on macOS.
- Architectural decisions are recorded as ADRs in `architecture/`; follow the `adr` skill.
