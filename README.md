# dassie

A small, personal micro-VM app host for a single Linux machine. A daemon turns
desired state into plain systemd units: one Caddy unit plus a unit per
Firecracker microVM, each VM running a single container. An app can have
several VMs at once, such as blue and green during a deploy or replicas for
high availability. systemd, not the daemon,
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
cargo test --workspace
cargo fmt --all --check
cargo lint-x86    # clippy for x86_64-unknown-linux-musl, -D warnings
cargo lint-arm    # clippy for aarch64-unknown-linux-musl, -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check  # advisories, licenses, bans, sources (deny.toml)
cargo shear       # unused dependencies
```

Install the extra tools with `cargo install --locked cargo-deny cargo-shear`.
`cargo lint-x86` and `cargo lint-arm` are aliases in `.cargo/config.toml`.
They lint the Linux-only code from macOS: clippy only type-checks, so no Linux
linker is needed. A plain `cargo clippy` on macOS skips everything behind
`cfg(target_os = "linux")`.

Lint policy lives in `[workspace.lints]` in `Cargo.toml`; warnings fail CI.
`unsafe` is denied by default: a crate that needs it opts in with
`#![allow(unsafe_code)]` at its root, and every `unsafe` block needs a
`// SAFETY:` comment. `dassie-init` (PID 1) and `dassie-proto` (parses
untrusted guest input) also deny `unwrap`, `expect`, `panic!` and indexing at
their crate roots.

CI also runs [actionlint](https://github.com/rhysd/actionlint) and
[zizmor](https://docs.zizmor.sh) on `.github/`.

### Fuzzing

`fuzz/` holds [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets
for `dassie-proto`. It is a workspace of its own, so the stable checks above
never build it. It needs nightly (`rustup toolchain install nightly`) and
`cargo install --locked cargo-fuzz`.

```sh
cargo +nightly fuzz run recv fuzz/corpus/recv fuzz/seeds/recv -- \
  -create_missing_dirs=1 -max_total_time=60 -max_len=131072 -dict=fuzz/proto.dict
cargo +nightly fuzz run roundtrip -- -max_total_time=60 -max_len=131072
```

- `recv` feeds arbitrary bytes to `FrameReader::recv` as `Event`, `Request`
  and `Response`. It checks that nothing panics, that only EOF and
  `LineTooLong` stop the stream, and that any accepted frame matches what
  `FrameWriter` would send (ADR 0004).
- `roundtrip` generates messages with the `arbitrary` feature of
  `dassie-proto`. It checks that every frame `FrameWriter::send` emits decodes
  back to the same value.

`-max_len` is above `MAX_LINE`, so oversized frames are reachable. Seeds from
ADR 0004 live in `fuzz/seeds/` and are committed. New inputs go to
`fuzz/corpus/`, which is ignored. Crashes are saved to `fuzz/artifacts/<target>/`.
Replay one with `cargo +nightly fuzz run <target> <file>`. CI runs each target
for 60 s on a pinned nightly and uploads crash inputs as an artifact.

Tests that need KVM, Firecracker, systemd or TAP devices run on an x86_64 Linux
GitHub Actions runner or in a local aarch64 Linux VM (Apple Silicon M3 or later
with macOS 15 or later for nested virtualisation).

## License

[MIT](LICENSE)
