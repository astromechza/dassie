# 0014. Apps, revisions and instances

Status: Proposed
Date: 2026-11-24

## Context

M2's daemon (#11) reconciles apps into systemd units and per-VM files, from
desired state in the store (ADR 0016). It needs its first resource kinds:
what an operator asks for, what a running VM is made from, and the VMs
themselves.

- **Everything a guest sees is fixed at boot.** Its rootfs (ADR 0007, ADR
  0009), its guest drive (ADR 0015) and its Firecracker config cannot change
  under a running VM, and each boot starts clean (ADR 0006). A running
  instance can be restarted, never edited.
- **Several VMs per app.** Replicas run side by side, and a change of version
  replaces them. Each needs its own address (ADR 0008) and its own jail uid
  (ADR 0013).
- **systemd holds the observed state.** A unit that is `active` means the app
  reported ready, and `ExecMainStatus` is its exit (ADR 0012).
- **Crashes must converge.** A daemon killed mid-change and restarted must
  reach the same state (#11).
- **M2 only.** Rollout settings, health gates, rollback, routes and image GC
  belong to later milestones (#13, #15). This ADR must not need replacing for
  them, only extending.

Kubernetes has the shape: a Deployment owns ReplicaSets, one per distinct pod
template, which own immutable Pods, and controllers act on the difference
between desired and observed state, with no journal of steps. We copy that
shape and leave out the rest.

## Decision

**Three kinds: apps own revisions, and revisions own instances.** An app is
like a Deployment, a revision like a ReplicaSet, an instance like a Pod. Each
is a spec table and a status table in the store (ADR 0016), created by schema
migration 2.

### App

Keyed by the app's name: `[a-z0-9][a-z0-9-]{0,31}`, chosen by the operator,
fixed for the app's life.

- **Spec:**
  - `kind`: `service`, the only kind for now. Jobs (#27) add another.
  - `replicas`: how many instances should be ready, 0 to 64.
  - `revision`: the hash of the revision the app should run.
- **Status:**
  - `deleting_since`: set when the operator deletes the app.
  - `rolling_since`: set while the app has instances of an older revision,
    cleared once it has none. A long-standing value is a stuck rollout.
  - `current_instances`: how many counted instances (below) run the app's
    revision.
  - `old_instances`: how many counted instances run an older revision.
  - `deleting_instances`: how many are being torn down, of any revision. They
    still hold their slots and memory until teardown is confirmed, so a count
    that stays up is a stuck teardown.
  - Each pass writes these from the rows it read, so they are as of the last
    pass. Together they cover every instance row the app has. How many are ready is
    systemd's to say (ADR 0016), so the CLI reads that from systemd rather
    than from status.

**Applying an app** that is being deleted is refused, with a typed error,
until the deletion finishes; a delete cannot be undone. Otherwise it builds
the template from the operator's spec, pulls its
image outside any store write (ADR 0016), creates the revision if no
revision has that hash, and writes the app's spec: its `revision` and its
`replicas`. That write requires the app's spec at the revision read before
the pull (or no app, for a new one), so an app deleted and recreated under
the same name meanwhile makes it `Stale`. It also reads the app's status in
the same transaction, and is refused, writing nothing, if a delete began
during the pull. An identical template reuses the existing revision, so applying
the same spec again changes nothing, while a change of `replicas` alone takes
effect without a new revision.

### Revision

Keyed by `<app>/<hash>`. App names cannot contain `/`, so the key is
unambiguous, and a scan of the prefix `<app>/` lists one app's revisions.

- **Spec:** the template, and nothing else. A revision never changes.
- **Status:** `created_at`.
- **Pruned** in the same write that leaves it neither the app's `revision` nor
  the revision of any instance. Rolling back means applying the old spec
  again, which gives the same hash. A history of past revisions comes with
  rollback in M4.

**The template is everything rendered into an instance's files.** A change to
any field is a new revision:

| Field | Type, bounds | Default | Lands in |
| --- | --- | --- | --- |
| `image` | `registry/repository[:tag]@sha256:…`, the platform manifest's digest | required | the rootfs, by digest |
| `vcpus` | 1 to 32 | 1 | `machine-config` |
| `memory_mib` | 128 to 32768 | 256 | `machine-config`, the unit's `MemoryMax=` |
| `env` | map of name to value | empty | the guest config (ADR 0015) |
| `entrypoint` | non-empty list of strings | the image's | the guest config |
| `cmd` | non-empty list of strings | the image's | the guest config |
| `readiness_port` | 1 to 65535 | as ADR 0005 says | the guest config |
| `sigterm_timeout_seconds` | 0 to 10 | 10 | the guest config |
| `network` | boolean | `true` | a NIC and a /31, or neither |

- **`image`** is pinned when the app is applied: the operator's reference is
  pulled (ADR 0007, ADR 0011) and written in `Reference`'s normalised form
  with the platform manifest's digest, so `nginx:1.27` becomes
  `docker.io/library/nginx:1.27@sha256:…`. The tag stays, so a revision can
  always be pulled again. Retagging the same digest is therefore a new
  revision.
- **`env`** names match `[A-Za-z_][A-Za-z0-9_]*` and do not start with
  `DASSIE_`, which init reserves. Values contain no NUL. Env holds no
  secrets: ADR 0015 delivers those differently.
- The names follow ADR 0015's guest config, so a field means the same thing
  in both.
- Unit tuning (start timeouts, restart delays, start limits) and other fields
  come later, each with a default.

**A revision's hash is the sha256, in hex, of its template's canonical JSON.**
Every field equal to its default is left out, then the template is
serialised through `serde_json::Value`, which sorts object keys (its
`preserve_order` feature stays off), with no whitespace.

- Spelling out a default, such as `"vcpus":1`, gives the same hash.
- A new field with a default leaves every stored hash alone, because no
  stored template contains it. A daemon upgrade therefore never rolls every
  app.
- Defaults are frozen once released. A different default needs a new field,
  or a migration that rewrites the affected templates and accepts the
  rollout.

### Instance

Keyed by a [ULID](https://github.com/ulid/spec) in its canonical 26-character
form. It is also the unit name (`fc@<id>`), the jailer `--id` and the guest's
`instance_id`. ULIDs sort by creation time, and carry no name that could go
stale.

- **Spec, set at creation and never changed:**
  - `app` and `revision`;
  - `slot`: 0 to 32767, below;
  - `kernel_digest` and `initrd_digest`: the host-wide guest assets present
    at creation. They record drift after an asset changes; they do not pin
    the files.
- **Status:** `deleting_since`, set when the instance is to go.

**A slot is one host-wide allocation.** It is ADR 0008's index, which gives
the TAP, /31 and MAC, and ADR 0013's uid and gid (`0x70000000 + slot`). It is
held whether or not the instance has a NIC.

- A new instance takes the lowest slot that no instance row holds, deleting
  or not, and that is not in use on the host (below), found by a scan in the
  write that creates it.
- The slot is freed with the row, which goes only once teardown is confirmed.
  So a slot is never reused while its TAP or chroot remains.
- At most 32768 instances, counting those being deleted.

### Reconcile

This ADR decides what a pass does. How it does it, rendering units and
files, calling systemd and scheduling passes, is the reconcile ADR's.

Each pass reads the rows, reads unit states from systemd, and takes the next
step. No step is recorded as in progress, and every step is safe to repeat.
An instance counts as **ready** when its unit is `active`. **Counted** below
means not being deleted.

**Replicas and rollouts: two rules per app**, with `R` its `replicas`:

1. **Create.** While the app's revision has fewer than `R` counted instances
   and the app has fewer than `R + 1` counted instances in all, create one
   instance of the app's revision. An instance being torn down is not
   counted, so a slow teardown never blocks a rollout. It does mean an app
   can briefly hold more than `R + 1` VMs' memory.
2. **Remove.** While the app has more than `R` counted instances, mark for
   deletion the first of them, in this order, that may go: older revisions
   before the app's revision, then not ready before ready, then oldest
   first. Stop when none may go.
   - An instance **may go** if it is not ready, or if more than `R` are
     ready.
   - An instance of the app's revision may go only while that revision has
     more than `R` counted instances, so rule 2 never undoes rule 1.

   Not ready first matters: without it, a crash-looping old instance, or a
   surplus one after a scale-down, would wait forever for a ready count that
   never rises above `R`. Skipping past an instance that must stay matters
   too: with `R = 1`, one ready old instance and two failed new ones, the
   surplus failed one goes while the old one keeps serving.

- **A change of revision rolls one instance at a time.** One new instance
  starts, and a ready old one goes only once the new one is ready. An old
  instance that is not ready may go sooner, since stopping it lowers no ready
  count. The daemon never stops a ready instance that would leave fewer than
  `R` ready, as far as it can see when it acts (below). A new instance that fails in the moment
  between that check and the stop can still leave the app short: that is a
  failure, which the deploy does not guard against. The surge of 1 and the
  unavailability of 0 are constants here; M4 makes them settings.
- **Scaling up** starts every missing instance at once. **Scaling down** is
  rule 2.
- **A revision that never becomes ready** holds one extra instance while the
  old ones keep serving. `rolling_since` shows it.
- **Deleting an app** marks every instance for deletion at once, then removes
  the app and its revisions once the last instance row has gone.

**A failed instance stays failed.** When systemd gives up on a unit (its
start limit, ADR 0012), the daemon does not reset or restart it, and does not
replace it. It still counts towards `R` and is not ready, so the app runs
short until the operator acts: deleting the instance lets rule 1 create a
fresh one, and applying a fixed spec rolls past it. An old revision's failed
instance is still removed by rule 2, as it is not ready.

**Instances.** A counted instance's directory, holding its files (ADR 0013,
ADR 0015) and its slot, is written first; then its TAP and chroot are made,
and its unit started. One being deleted is undone in reverse: unit stopped,
then TAP and chroot removed, then its directory, and then its row deleted,
which frees its slot. So an instance's directory exists whenever any of its
other resources might.

**A removal is checked again before it stops a unit.** Rule 2 decides on
unit states the pass read, and a unit can fail after that. So just before
stopping an instance that rule 2 marked, the pass reads the app's spec and
unit states again, and applies rule 2 to that instance afresh: the app must
still have more than `R` counted instances, counting this one, with `R` as
it is now, and the instance must still be one that may go. Otherwise it
withdraws the mark instead. So a mark made under an older `R`, or one a
scale-up has since made unneeded, is never acted on. That narrows the window to the moment between the check and the
stop. An instance that fails after an old one has stopped is a failure like
any other, not a deploy. Instances marked because their app is being deleted
are stopped regardless.

**What the host holds.** ADR 0016 requires that no host-wide value is reused
while the host still carries it, including after a restore that left the
store behind the host. Each pass, before it creates an instance, lists
dassie's host resources: `fc@` units, `dt…` TAPs, per-instance directories
and jail chroots. Units, directories and chroots are named by instance id;
a TAP's name gives its slot. An instance without a NIC has no TAP, so the
daemon also writes each instance's slot into its own directory, where the
scan reads it. An orphan whose slot cannot be read blocks every new slot
until it has gone.

- **Directories claim slots, not rows.** A slot is in use on the host while
  any directory, the daemon's or an M1 one, records it. A TAP is told apart
  by the slot it carries, not its name, since both are `dt<slot>` (the nft
  policy matches `dt*`). A TAP whose slot no directory records is an orphan,
  whatever the rows say, and is torn down: the build and teardown orders
  above mean no live instance is ever without its directory.
- A unit, directory or chroot named by an id that no instance row holds is an
  orphan, and is torn down.
- A new instance takes no slot that is in use on the host.
- **An instance starts only on a slot nothing else uses.** If its slot is
  recorded by another directory, or carried by a TAP no directory claims, it
  waits until the other resources have gone. A restored row whose slot an M1
  VM still uses waits until the operator removes that VM.
- VMs deployed by hand in M1 have units and directories whose names are not
  ULIDs (`fc@web`, `vms/web`). They are not the daemon's: never touched, and
  reported. Their slots are read from their `net.env` and `jail.env`. The
  operator removes them when moving a host to the daemon.

### Writes

Following ADR 0016, a pass does no I/O inside a write. It records intent,
acts, then records the result, with preconditions:

- creating an instance row requires the app's spec at the revision the pass
  read, so a pass never acts on an app that changed under it;
- marking an instance for deletion under rule 2 requires both its row and
  the app's spec at the revisions read, since the decision rests on the
  app's `replicas`;
- deleting an instance's row after teardown requires its row at the revision
  read;
- a `Stale` result starts a new pass. Passes for one app never overlap.

## Consequences

- A crash at any point converges. After a restart the rows still say what
  should exist, systemd says what does, and the host scan finds what was
  left behind. That meets #11's crash-replay test with no journal.
- Re-applying the same spec is a no-op by construction, which covers most of
  #11's idempotency test.
- Every template change is a rollout, including a changed timeout. A boot
  takes about 1.5 s, so this is cheap, and it keeps an instance's files a
  pure function of its row.
- A deploy takes about `R` times an instance's time to boot and become ready.
- A failed instance needs an operator. The daemon never loops on a broken
  revision, but an app can run short of replicas until someone deletes the
  instance or applies a fix.
- A rollout always moves to new addresses, since new instances get new
  slots. Routes, when they arrive, must follow instances, not apps.
- Revisions do not outlive their use, so there is no rollback history yet.
  M6's image GC can keep every digest of a revision that still exists.
- The rendered files depend on the daemon's code as well as the row. Whether
  a daemon upgrade re-renders existing instances, or only new ones, is left
  to the reconcile ADR.
- Deferred, each an extension of these kinds rather than a replacement:
  rollout settings, health gates and rollback (M4), routes, async pulls, image
  GC and retention (M6), jobs (#27), unit tuning fields, and moving running
  instances onto new guest assets.
- `deploy-image.sh`, `net-env.sh`, `jail-env.sh` and the hand-picked index go
  once the daemon renders instances from these rows.
