# 0011. Registry pull retries

Status: Accepted
Date: 2026-11-14

## Context

ADR 0007's registry client makes each request once: "There are no retries."
Any transient failure fails the whole pull: a connection reset, a read timeout
partway through a layer, a 5xx from the registry or its CDN, or Docker Hub's
429. Blobs already stored are kept, so a re-run resumes from the next missing
blob, but a large layer starts again from zero. The daemon (M2) would also
see a transient failure as a failed deploy.

## Decision

**`Client` retries a manifest or blob GET that fails transiently.** This
supersedes ADR 0007's "There are no retries". Everything else in ADR 0007's
client stands: the timeouts, the token renewal on a 401, and the bounds.

- **Retried:** any HTTP-layer error (I/O, timeout, connection), a blob body
  that broke off while it was being stored, and HTTP 408, 429 and 5xx. The
  few HTTP-layer errors that will fail the same way again (TLS, bad URL) are
  retried as well, rather than matching on ureq's error kinds; the cost is
  the pauses.
- **Not retried:** other 4xx, a wrong digest or size, and unsupported
  layers. A mismatch means the registry served the wrong bytes, and asking
  again will not fix that.
- **Policy:** a `Retry { attempts, delay }` setting on `Client`. The default
  is 3 attempts in all, pausing 1 s and then 2 s (each pause doubles the
  last). Tests use no pause.
- **A blob restarts from its first byte.** `Store::put` already leaves
  nothing behind on failure, so a retry needs no cleanup.
- **Progress:** `Event::Retrying` reports each retry, with the blob (none for
  a manifest), the attempt about to start and the error. A retried blob's
  `Fetching` starts again from 0.
- **The 401 token renewal is not a retry,** and does not use up an attempt.

## Consequences

- Short outages, broken connections and brief rate limits no longer fail a
  pull or a deploy. A failing request costs up to 3 s of pauses, plus the
  repeated attempts themselves, which for a blob can mean downloading it
  again before giving up.
- **No `Retry-After` and no jitter.** Docker Hub's 429 is a pull quota with a
  window of hours, so honouring the header up to any sensible cap would rarely
  change the result. One host pulls one image at a time, so there are no other
  clients for jitter to spread out. Either can be added inside `Retry`
  without changing callers.
- **No resume with `Range`.** A very large layer that keeps breaking off near
  its end downloads in full each time. Resuming would need `Store::put` to
  keep its temporary file and hash state across attempts; that is left until
  large layers show it is needed.
