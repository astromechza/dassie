# 0004. Host <-> guest wire protocol

Status: Accepted
Date: 2026-11-11

## Context

Every guest's `dassie-init` reports lifecycle events to the host, and the host
needs to ask a guest for its state and to stop it gracefully. This protocol is
the daemon's contract with every guest (ADR 0002). Settling it before the
daemon exists keeps reconcile logic off a moving target. `dassie-proto` must
stay sync and small for the static musl init: serde plus one encoding.

Firecracker exposes guest vsock to the host as Unix sockets. For a
guest-initiated connection to the host (CID 2) on port P, Firecracker connects
to the host socket `<uds_path>_P`. For a host-initiated connection, the host
connects to `<uds_path>`, writes `CONNECT P\n` and reads `OK <n>\n` (or the
connection is closed if nothing listens on P).

`systemctl stop` kills Firecracker without the guest noticing, and
Ctrl-Alt-Del through the Firecracker API exists only on x86_64. A graceful stop
therefore needs its own request.

## Decision

**Framing.** Newline-delimited JSON. Each frame is one compact UTF-8 JSON
object terminated by `\n`, at most 65536 bytes including the terminator. A
longer frame is a framing error and the receiver closes the connection. A blank
line is an invalid frame. Senders do not emit `\r`; receivers do not strip or
special-case it. Every frame starts with two envelope fields:

- `v`: protocol version, currently `1`.
- `type`: snake_case message name.

Receivers ignore unknown fields. Optional fields are omitted when absent,
never `null`. Adding a message type or an optional field is non-breaking and
stays `v: 1`. `v` changes only for breaking changes: removing or renaming a
field, or changing a field's type or meaning.

**Channels.**

| Channel | Direction | Guest | Host |
| --- | --- | --- | --- |
| events | guest -> host, one-way | connects to CID 2 port 1024 | listens on `<uds_path>_1024` |
| control | host -> guest, request/response | listens on port 1025 | connects to `<uds_path>`, `CONNECT 1025` |

**Events** carry no VM identity: each VM has its own `uds_path`, so the host
already knows which VM a connection belongs to.

| `type` | Fields | Sent when |
| --- | --- | --- |
| `started` | `pid` (u32) | The entrypoint was spawned. A new `started` also means the VM rebooted. |
| `ready` | none | The readiness probe succeeded, or right after `started` when no port is declared. At most once per boot. |
| `exited` | exactly one of `code` (i32) or `signal` (i32) | The entrypoint exited; the VM reboots next. |

```json
{"v":1,"type":"started","pid":42}
{"v":1,"type":"ready"}
{"v":1,"type":"exited","code":3}
{"v":1,"type":"exited","signal":9}
```

The guest never reads from the events connection.

**Control.** The host sends one request line and the guest answers with
exactly one response line, in order. A connection can carry several
requests, and either side may close it. A malformed request gets an `error`
response and the connection stays open; a framing error closes it.

| Request | Fields | Meaning |
| --- | --- | --- |
| `health` | none | Report the lifecycle state. |
| `stop` | `timeout_ms` (u64, optional; guest default 10 s) | SIGTERM the entrypoint, then SIGKILL after the timeout. Idempotent: a repeat does not restart or extend the deadline. |

| Response | Fields | Meaning |
| --- | --- | --- |
| `health` | `state`: `starting` \| `ready` \| `stopping` \| `exited`; `pid` (u32) | Answer to `health` and `stop`. |
| `error` | `code`: `unsupported` \| `bad_request`; `message` (string) | `unsupported`: unknown request `type`. `bad_request`: invalid JSON, bad fields or unsupported `v`. |

```json
{"v":1,"type":"health"}
{"v":1,"type":"health","state":"ready","pid":42}
{"v":1,"type":"stop","timeout_ms":5000}
{"v":1,"type":"health","state":"stopping","pid":42}
{"v":1,"type":"reload"}
{"v":1,"type":"error","code":"unsupported","message":"unknown request type \"reload\""}
```

**Delivery.** Events are best effort. While disconnected, the guest holds its
pending events (at most three per boot), so `started` survives a listener
that starts after the VM. A failed write keeps the event pending, and the guest
reconnects, retrying every 500 ms. On a Unix socket or vsock, a write to a
closed peer fails outright, so the retry does not duplicate anything a host
read. An event written into a connection the host then drops without reading
it is lost. On entrypoint exit the guest tries to deliver `exited` for up to
2 s, then reboots regardless. Lost events are tolerated: a `health` request is
the source of truth.

**Reserved for later**, named here so additions stay additive:

- Events: `restored` (snapshot restore, M5), `failed` (init error before
  `started`), `unready`, `stopping`, `hello` (init and protocol versions, boot
  id), stats.
- Requests: `signal`, `quiesce` / `resume` (snapshots), `reidentify` (a clone's
  new identity, clock and entropy), `stats`, `secrets`.

The Firecracker `CONNECT` handshake is a Firecracker detail rather than part of
this protocol. It lives in the `dassie-proto` listener example until the
daemon needs it, then moves to `dassie-vm`.

## Consequences

- The protocol can be debugged with `socat`/`nc` and `jq`, and needs only
  `serde_json` in the guest.
- Older guests answer newer requests with `unsupported`, and older hosts skip
  newer events, so either side can be upgraded first without bumping `v`.
- The host cannot rely on seeing every event. Anything that must be correct,
  such as health gating in M4, polls `health`.
- `stop` works on both architectures. The host can stop a VM gracefully without
  the Firecracker API, at the cost of the guest running a control listener.
- Secrets must not ride on the kernel cmdline, because the app can read
  `/proc/cmdline`. A future `secrets` request or drive is the place for them.
