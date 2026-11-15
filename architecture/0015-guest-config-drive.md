# 0015. Guest config drive

Status: Accepted
Date: 2026-11-15

## Context

ADR 0005 delivers per-VM guest config as `dassie.*` kernel cmdline args:
the instance id, the colour, env overrides, the readiness port, the stop
timeout and, since ADR 0008, the DNS resolvers. That channel is too small
and too weak for M2:

- the kernel cmdline is capped at 2048 bytes for everything, kernel args
  included, so a handful of env vars fills it;
- it has no escapes: a value can never contain `"`, and whitespace needs the
  kernel's quotes;
- the M2 store (ADR 0014) lets each revision override the image's
  entrypoint and cmd, k8s-style, with lists of arbitrary strings, which the
  cmdline cannot carry faithfully;
- every process in the guest can read it through `/proc/cmdline`.

Later milestones also want files in the guest: secrets as files and mounted
config assets, as k8s Secret and ConfigMap volumes give a pod. Whatever
replaces the cmdline should not need replacing again for those.

Firecracker can attach more virtio-blk drives, the guest kernel already
mounts erofs (ADR 0009), and the host already builds deterministic erofs
images with `mkfs.erofs --tar=f` (ADR 0007). The jailer setup hard-links
every drive into the jail, so a drive file has to sit on the jail's
filesystem (ADR 0013).

## Decision

**Per-VM guest config travels on a read-only guest drive, not the
cmdline.** Every VM gets a second drive holding a small erofs with one file,
`/guest.json`. The `dassie.*` cmdline args are gone. ("Guest" keeps it
apart from `config.json`, which is Firecracker's config.)

**What moves and what stays.**

- On the drive: the instance id, env, `entrypoint`, `cmd`, readiness
  port, SIGTERM timeout and nameservers, plus the new `hostname`.
- Dropped: `dassie.colour`. Nothing used it, and M4 keeps blue/green state
  in the daemon and Caddy, not the guest. It can come back as a new field
  if an app ever needs it.
- On the cmdline: the kernel's own args, `root=/dev/vda ro`
  (`rootfstype=` optional, ADR 0006), and `ip=` for the guest address
  (ADR 0008). The kernel applies `ip=` before init runs, and init has no
  netlink code to do it instead.

**`guest.json`, version 1.** One UTF-8 JSON object, at most 1 MiB:

```json
{
  "version": 1,
  "instance_id": "01JD3S2MZ0Q5W4T8E6Y7R9B1C2",
  "hostname": "web-01jd3s2m",
  "env": { "GREETING": "hello \"world\"" },
  "entrypoint": ["/bin/server"],
  "cmd": ["--listen", ":8080"],
  "readiness_port": 8080,
  "sigterm_timeout_seconds": 10,
  "nameservers": ["1.1.1.1", "9.9.9.9"]
}
```

| Field | Type | Required | Effect |
| --- | --- | --- | --- |
| `version` | integer | yes | Must be `1`. |
| `instance_id` | string | yes | Sets `DASSIE_INSTANCE_ID` in the app env. Same character rules as `hostname`, which it defaults. |
| `hostname` | string | no | The guest's hostname, 1–64 ASCII letters, digits, `-` or `.`, and not a number or address (below). Unset: `instance_id`. |
| `env` | object of string to string | no | Merged into the image `Env` by key. |
| `entrypoint` | list of strings | no | Replaces the image `Entrypoint`. Empty means unset. |
| `cmd` | list of strings | no | Replaces the image `Cmd`. Empty means unset. |
| `readiness_port` | integer 1..65535 | no | TCP port whose connect means ready. Unset: the lowest `ExposedPorts` tcp port, else ready on spawn (ADR 0005). |
| `sigterm_timeout_seconds` | integer 0..10 | no | Grace period between SIGTERM and SIGKILL. Unset: 10 s. |
| `nameservers` | list of up to 3 IP strings | no | Resolvers for `/etc/resolv.conf`. Unset: the image's file stays. |

`entrypoint` and `cmd` are named after the OCI image config fields they
replace.

- The host omits unset fields; it never writes `null`.
- **Strict.** Init checks `version` first, so a newer format fails with
  "unsupported guest config version 2", then refuses any field it does not
  know. A newer host never has a field such as `entrypoint` silently
  ignored by an older init. Adding a field therefore needs an init that
  knows it; the two ship together from this repository. A breaking change
  bumps `version`.
- **What `execve` can carry.** No string may contain NUL. Env keys must be
  non-empty and contain no `=`. Each env entry (`KEY=value`) and each
  argument must be under 128 KiB, Linux's `MAX_ARG_STRLEN`. Together they
  must fit in 1 MiB, counting each string's NUL and its 8-byte pointer,
  which leaves the other half of the 2 MiB `execve` budget (below) to the
  image's own `Env` and `Cmd`. So 250,000 empty arguments, which fit in a
  small `guest.json`, are refused too. A config that would fail with
  `E2BIG` is refused when the drive is written, not at boot, unless the
  image's own share is what overflows. The store's stricter rules (key syntax, no `DASSIE_*` keys,
  ADR 0014) are the daemon's to enforce. `DASSIE_INSTANCE_ID` is set after
  `env`, so it wins regardless.
- **No numeric hostnames.** A name whose every dot-separated label is a
  number (decimal, octal or `0x` hex), such as `127.0.0.1` or `10`, is
  refused as `hostname`, and as `instance_id` too since it is the default.
  `getaddrinfo` reads such a name as an address and never looks in
  `/etc/hosts`, so the mapping below would not apply.
- **Why 1 MiB.** The drive itself has no practical limit. The cap is a
  choice, for three reasons:
  - Nearly all of `guest.json` ends up in the app's `execve`, and Linux
    caps argv plus env together at a quarter of the stack rlimit: 2 MiB
    with the default 8 MiB. Values much past 1 MiB could never reach the
    app, and the rest of that budget is the image's own `Env` and `Cmd`.
  - PID 1 reads the whole file into memory, so a bound keeps a bad file
    from costing a small guest its RAM.
  - It matches Kubernetes' 1 MiB for a ConfigMap or Secret, which M2's
    users will already know.
  The cap covers `guest.json` only. Files under `files/` (below) are not
  passed through `execve` and will get bounds of their own.
- **Why at most 10 s.** vm-run SIGKILLs Firecracker 15 s after its stop
  request (ADR 0012), whatever the guest's grace period. A longer period
  would be cut short without a word, as `dassie.stop_timeout` already was,
  so the schema refuses it instead. 10 s leaves 5 s for the guest to sync
  and reboot. Raising the cap needs vm-run to learn the value first.
- The schema and its checks live in `dassie-proto` (`guest_config`), shared
  by init and the drive writer, and are fuzzed with the rest of
  init's config loading.

**Merge rules are ADR 0005's.** Drive values win over the image config. Env
merges by key, replacing image values in place. argv follows Kubernetes'
`command` and `args` rules, which are also what `docker run --entrypoint`
does (a new entrypoint drops the image's `Cmd`):

| `entrypoint` | `cmd` | argv |
| --- | --- | --- |
| unset | unset | image `Entrypoint` + image `Cmd` |
| set | unset | `entrypoint` |
| unset | set | image `Entrypoint` + `cmd` |
| set | set | `entrypoint` + `cmd` |

An empty argv is a boot error. There is no `$(VAR)` expansion.

**The drive.**

- **Per instance.** It carries per-instance values (the instance id and the
  nameservers), so each instance has its own files, next to its
  Firecracker `config.json`:
  - `/var/lib/dassie/vms/<id>/guest.json`, the config itself, readable on
    the host without erofs tools;
  - `/var/lib/dassie/vms/<id>/guest.drive`, the erofs built from it.

  That is the jail's filesystem, so ADR 0013's hard link works. vm-run
  links every drive, and this one becomes `/drive-guest`.
- **Attached second and read-only**, as `drive_id: "guest"` after
  `rootfs`, with `is_read_only: true`. Firecracker attaches drives in
  config order, so it is always `/dev/vdb` in the guest. A swapped order
  still fails loudly: neither drive holds the other's files.
- **Built** by `dassie guest-drive --out FILE < guest.json`. It decodes the
  JSON with the same strict checks init runs, then runs
  `mkfs.erofs --tar=f` on a tar holding a `0755 root` root and `guest.json`
  (`0400 root`). The superblock time is fixed and the UUID comes from the
  tar's sha256, so the same config always builds the same bytes and a
  redeploy of unchanged config restarts nothing. No compression: the file
  is a few KiB. The daemon calls the same code. The CLI takes JSON rather
  than a flag per field, so it does not change when the schema does.
- **Who fills it.** Until the daemon does (M2), `deploy-image.sh` writes
  `guest.json` from its arguments: `instance_id` is the VM name and
  `nameservers` is `DASSIE_DNS` from the VM's `net.env` (ADR 0008), split
  on commas. `net-env.sh` already limits that to 3 addresses, and
  `vm-config.sh` stops emitting `dassie.dns=`. A VM without `net.env` gets
  no `nameservers`, and keeps the image's `/etc/resolv.conf`.
- **Modes.** Both files are `0640 root:<jail gid>` on the host, so only
  that VM's jailed Firecracker and root can read them.

**Init.**

1. Before the switch-root (ADR 0006), init waits up to 5 s for `/dev/vdb`,
   mounts it read-only (`nosuid,nodev,noexec`) as erofs on `/guest` in the
   initramfs and reads `/guest/guest.json`. After the switch-root that
   mount is out of the app's reach, like `/lower`.
2. After the switch-root, init sets the hostname (`hostname`, else
   `instance_id`) with `sethostname`, and replaces `/etc/hostname` and
   `/etc/hosts` in the app root on every boot, as Docker and the kubelet do
   for a container. `/etc/hosts` holds the usual `localhost` lines and maps
   the hostname to the guest address from `ip=` (its first field), so an
   app that binds to its own name is reachable from the host. A VM with no
   NIC maps it to `127.0.1.1`, as Debian does. The readiness probe tries
   that address as well as `127.0.0.1` (amending ADR 0005), so such an app
   still becomes ready.
3. A missing drive, a mount failure, a missing, oversized or invalid
   `guest.json`, an unknown field or an unknown `version` is a boot error,
   logged on the console before the reboot, as a bad `dassie.*` value was.
4. Any `dassie.*` arg left on the cmdline is a boot error too ("moved to the
   guest drive"), so an old host with a new init fails loudly instead of
   having its env dropped.

**Reserved, not built.** Files beyond `guest.json` go in the same erofs
under `files/`, with their modes and owners set in the tar, for init to
bind-mount into the app root at paths `guest.json` will name.

**Secrets: intended, not built.** A later ADR builds them; this one fixes
the direction so nothing built now gets in the way:

- **Never in `guest.json`.** `guest.json` is plain config: readable on the
  host for debugging, printed by tools, and safe to show in a diff. So a
  secret value never goes in its `env` or any other field, and the
  daemon never copies one there.
- **Files by default.** Secret values go on the same guest drive under
  `secrets/<name>`, `0400 root` in the tar. Before the spawn, init copies
  each to a tmpfs at `/run/secrets/<name>`, mode `0400` and owned by the
  app's user, as Docker and Kubernetes mount them. `guest.json` names the
  secrets but never holds their values. A file is read only by code that
  asks for it, is not inherited by child processes, and does not end up in
  crash dumps or `/proc/<pid>/environ`.
- **Env only by opt-in.** Many images read config only from env, so a
  `guest.json` field will map an env var to a secret by name (as a
  Kubernetes `secretKeyRef`), and init reads the value from the drive at
  spawn. The value is still never in `guest.json`, but it is then in the
  app's environment, visible to its uid in `/proc/<pid>/environ` and
  inherited by everything it starts. That is the app owner's trade-off to
  make per variable, not a default.
- **Open for that ADR:** secrets at rest on the host (the drive sits in
  `vms/<id>/`, on the jail's filesystem for the hard link, unless both
  move to a tmpfs), their size cap, and rotation, which with a drive means
  a restart unless vsock later pushes new values.

**Why not vsock.** init could instead fetch its config from vm-run over a
new vsock port before it launches the app. That needs no drive and no
`mkfs.erofs`, but every boot would then depend on a live host peer, a
1 MiB config would need frames past ADR 0004's 64 KiB, and files would have
to be copied into guest RAM rather than paged from a drive. A later push of
changed values while the VM runs (secret rotation, say) can still use vsock
next to the drive.

**Cut over in one change.** Nothing outside dev hosts runs dassie yet, so
there is no transition period with both channels.

This supersedes the cmdline half of ADR 0005's config delivery and its
`dassie.dns` amendment. ADR 0005's
image config, merge, readiness, supervision and exit rules stand.

## Consequences

- Config size is bounded by the 1 MiB cap, not by 2048 bytes shared with
  the kernel, and values are arbitrary strings: quotes, spaces, `=`,
  newlines.
- The app no longer sees its config in `/proc/cmdline`. `/dev/vdb` stays in
  the guest's devtmpfs as `root:disk 0660`, so a non-root app cannot read
  the drive and a root app can. Secrets are not built yet and must never
  go in `guest.json`'s `env`; the section above says how they will travel.
- The app's env var is now `DASSIE_INSTANCE_ID`, not `DASSIE_INSTANCE`, and
  there is no `DASSIE_COLOUR`.
- Each VM costs one more virtio-blk device and a few KiB on disk, and each
  deploy one `mkfs.erofs` run of a few milliseconds. The host keeps needing
  erofs-utils 1.8, as image builds already do.
- `guest.json` and `guest.drive` are two copies of one config. The drive is
  always built from the JSON in the same step, so a mismatch takes a hand
  edit of one of them. Nothing on the host reads `guest.json` at runtime.
- A new host with an old init boots the image's defaults, because an old
  init never looks at the drive. Init and dassie must be upgraded together;
  the reverse mismatch fails loudly.
- Adding a config field needs a new init before any host may send it.
- The drive is written before the VM starts and never changes underneath
  it: changing config means a new drive and a restart, which M2's
  revisions already imply.
- A bad config is a boot error with no event of its own. vm-run exits 196,
  as for a lost `exited` event (ADR 0012), and `Restart=always` retries
  until systemd's start limit fails the unit. The cause is only in the
  console text in the journal. The drive writer runs the same checks, so
  this takes a version mismatch or a corrupt file.
- Apps that need more than 10 s to drain cannot have it yet.
- Every guest has a real hostname that resolves inside it, with or without
  a NIC, which `ip=`'s hostname slot could not give. An image's own
  `/etc/hostname` and `/etc/hosts` are always replaced, not merged.
- The guest still gets its address from `ip=`. Moving networking onto the
  drive would need netlink code in init.
