# 0002. Cargo workspace crate layout

Status: Accepted
Date: 2026-11-11

## Context

Dassie has a host daemon (reconcile loop, SQLite, systemd, Caddy), a CLI that
talks to it over a Unix socket, Firecracker VM operations, an OCI-to-rootfs
builder, and a guest PID 1 that runs one container entrypoint per microVM. The
guest init is a static musl binary, so anything it shares with the host must
stay small and free of an async runtime.

We considered using kata-agent as the guest agent. kata-agent is host-driven:
it speaks ttRPC/protobuf over vsock and waits for the host to push an OCI spec
via CreateContainer/StartContainer. Dassie's model is the opposite: the guest
init runs a baked-in entrypoint, VM exit equals container exit, and systemd
supervises the VM with no daemon needed to reach steady state. Adopting
kata-agent would change the VM crate, the rootfs builder and the lifecycle, not
just the protocol.

## Decision

A single Cargo workspace with five crates under `crates/`:

| Crate | Kind | Owns |
| --- | --- | --- |
| `dassie` | bin | Daemon and CLI (subcommands of one binary). SQLite, systemd units and D-Bus, Caddy config. Composes the other crates into app-level start / clone / stop. |
| `dassie-vm` | lib | Firecracker-facing only: Firecracker and jailer config, the Firecracker API socket (pause, snapshot, restore). No systemd knowledge. |
| `dassie-image` | lib | OCI image to ext4 guest rootfs. |
| `dassie-proto` | lib | Dassie's own host <-> guest protocol: message types and framing, generic over `Read + Write`. Dependency budget: serde plus one small encoding, no async runtime. |
| `dassie-init` | bin | Guest PID 1, static musl. |

Intended internal dependencies: `dassie` -> `dassie-vm`, `dassie-image`,
`dassie-proto`; `dassie-init` -> `dassie-proto`; the libraries depend on no
other workspace crate. Path dependencies are added when code first uses them.

We write our own guest init and protocol rather than using kata-agent.

## Consequences

- There is one host artefact to install; the CLI shares types with the daemon
  without a separate client crate.
- systemd concerns stay in `dassie`, so `dassie-vm` can be tested against a fake
  Firecracker API without any service manager.
- `dassie-proto` is the contract with every guest; keeping it sync and small
  keeps `dassie-init` small.
- If kata-agent is ever revisited, it would be an alternative guest-agent
  backend inside `dassie-vm` plus a different rootfs flow, not a swap of
  `dassie-proto`.
