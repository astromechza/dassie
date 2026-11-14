# 0010. Ingress through Caddy

Status: Accepted
Date: 2026-11-13

## Context

Apps need HTTPS ingress by hostname. ADR 0008 already allows host → VM
traffic, so a proxy on the host reaches every guest by its address with no
extra nft rule. The README and ADR 0002 name Caddy as that proxy, run as one
systemd unit.

What is still open is who owns Caddy's config, and how it changes:

- **No daemon at runtime.** As with `fc@.service` and the TAPs, ingress must
  come back after a host reboot from units and files alone.
- **One source of truth.** Caddy can save its own copy of every config it is
  given (`autosave.json`) and reload that at start. That copy and our file
  would then disagree after any change made through the API.
- **Later milestones.** M2's daemon generates the config. M4 swaps a route from
  blue to green behind a health gate, which wants a cheap change to one route.
- **The admin API can do anything to the proxy.** It must not be reachable
  from the network, or by any local user.

M1 drives all of this by hand, from `scripts/host/deploy-image.sh`. This ADR
records what M2 inherits, and not that script.

## Decision

- **One `caddy.service`**, running a pinned upstream Caddy release
  (`scripts/host/caddy.sh`) under `DynamicUser`, with only
  `CAP_NET_BIND_SERVICE`. It is not ordered against any VM: a route to a VM
  that is not up yet answers 502 until it is.
- **`/etc/dassie/caddy.json` is the whole config and its only source of
  truth.** It is native Caddy JSON, not a Caddyfile, so it is generated rather
  than adapted. `admin.config.persist` is `false`, so Caddy never writes its
  own copy, and `caddy run --config` loads the file at every start.
- **Changes apply as a whole-file reload through the admin API on a Unix
  socket**, `/run/caddy/admin.sock` (`systemctl reload caddy`, i.e.
  `caddy reload --address unix//run/caddy/admin.sock`). There is no admin
  listener on TCP. A reload is graceful and atomic: Caddy keeps serving the old
  config until the new one has loaded, and keeps it if the new one fails.
- **One route per VM**, with `"@id": "vm-<name>"`, matching its hostname and
  reverse-proxying to `<guest IP>:<port>`. The `@id` lets M4 change one route
  through the API, as long as the same change goes into the file.
- **`tls internal`.** One automation policy issues every certificate from
  Caddy's own local CA, kept in `storage` at `/var/lib/caddy`.
  `install_trust: false` keeps that CA out of the host's trust store. Clients
  trust it explicitly (`curl --cacert /var/lib/caddy/pki/authorities/local/root.crt`).
  The server is TLS-only (`tls_connection_policies: [{}]`); automatic HTTPS
  adds the HTTP → HTTPS redirect on :80.

## Consequences

- A reboot, a crash or a `systemctl restart caddy` all serve exactly what the
  file says. Nothing has to replay API calls.
- Whoever writes the file must write it atomically and durably: temp file,
  fsync, rename, fsync the directory. The M1 power-cycle drill showed a
  rename without fsync coming back empty.
- Every change re-provisions the whole config. That is cheap for M1's handful
  of routes. M4 may PATCH one route by `@id` for a faster swap, and must then
  write the file too.
- Only root and the unit's own user can reach the admin socket.
- Clients must be given the local root CA. Public certificates (ACME) need a
  different issuer policy and inbound :80/:443 from the internet, and are out
  of scope.
- Caddy does not health-check upstreams. Readiness and health stay on vsock
  (ADR 0004, ADR 0005); M4's health gate decides when a route moves.
