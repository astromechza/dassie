# 0018. The host closes the events connection after `exited`

Status: Accepted
Date: 2026-11-24

## Context

ADR 0004 has the guest try to deliver `exited` for up to 2 s and then
reboot, and ADR 0012 turns a missing `exited` into vm-run's exit status 196.
In practice, init counted `exited` as delivered as soon as its `write`
returned. That only means the bytes reached the guest kernel's vsock socket.
The guest kernel still has to put them on the virtio queue, and Firecracker
still has to forward them to the host socket on its own thread. Init reboots
straight after the write. The reboot resets the vsock device, and the
Firecracker process exits. Either can happen before the bytes are
forwarded, and then `exited` is lost.

CI's `kvm-boot` hit this on a jailed boot. testapp exited with code 3, and
Firecracker logged a vsock event on a device that was no longer active just
before the reboot. vm-run had received `started` and `ready` on the same
connection, never got `exited`, and exited 196. vm-run's drain (ADR 0012)
cannot recover bytes that never left the VM.

Only the host knows when it has read `exited`, so the host has to say so.

## Decision

- Once vm-run's events thread has forwarded `exited` to the main loop, it
  closes that connection. `exited` is the last event of a boot, so nothing
  more is coming.
- After writing `exited`, init reads from the events connection until it
  gets EOF or an error, which is the host's close coming through
  Firecracker. Only then does it reboot. It still gives up after 2 s and
  reboots anyway, logging `could not deliver exited event`.
- The close is the acknowledgement. The host sends no data, and no new
  message type is added, so the protocol stays `v: 1`.
- The `dassie-vm` listener example closes after `exited` too.
- The boot smoke test fails if the guest logs that it saw no close.

## Consequences

- When init reboots, vm-run has already read `exited`, so a 196 now means
  the guest really did not report one: a crash, a panic, or a host that
  stopped reading.
- An older host never closes, so a newer guest waits the full 2 s before it
  reboots. An older guest never reads, so a newer host's close changes
  nothing for it. Either side can be upgraded first.
- The guest cannot tell this close from a host that dropped the connection
  for another reason, such as a framing error or a vm-run crash. If that
  happens while `exited` is in flight, the event is lost as before, and the
  result is a 196.
- The guest now reads from the events connection, which ADR 0004 ruled out.
  It reads only to see the close.
