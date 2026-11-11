# dassie

A small, personal micro-VM app host for a single Linux machine. A daemon turns
desired state into plain systemd units: one Caddy unit plus one Firecracker
microVM per app, each running a single container. systemd, not the daemon,
keeps everything alive, so a reboot or daemon crash returns to the same steady
state. The daemon exists for high availability and blue/green deploys via Caddy
route changes.

Status: early scaffolding, nothing runs yet.

## Crates

| Crate | Role |
| --- | --- |
| [`dassie`](crates/dassie) | Host daemon and CLI: reconcile loop, SQLite, systemd, Caddy |
| [`dassie-vm`](crates/dassie-vm) | Firecracker and jailer config, Firecracker API socket |
| [`dassie-image`](crates/dassie-image) | OCI image to ext4 guest rootfs |
| [`dassie-proto`](crates/dassie-proto) | Host <-> guest protocol over vsock |
| [`dassie-init`](crates/dassie-init) | Guest PID 1 (static musl) |

Design decisions are recorded in [architecture/](architecture/).

## Development

macOS is the development machine; dassie targets Linux only
(`x86_64-unknown-linux-musl` primary, `aarch64-unknown-linux-musl` secondary).
The toolchain and targets are pinned in `rust-toolchain.toml`.

```sh
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --target x86_64-unknown-linux-musl
cargo check --workspace --target aarch64-unknown-linux-musl
```

Tests that need KVM, Firecracker, systemd or TAP devices run on an x86_64 Linux
GitHub Actions runner or in a local aarch64 Linux VM (Apple Silicon M3 or later
with macOS 15 or later for nested virtualisation).

## License

[MIT](LICENSE)
