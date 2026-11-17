# 0016. Store data access layer

Status: Proposed
Date: 2026-11-17

## Context

M2's daemon (#11) needs one durable store for the state it owns: what each
app should run, what it has allocated to each VM (ADR 0008's index, ADR
0013's uid), and how far it has got. #11 planned SQLite. Several forces
point elsewhere:

- **No C toolchain to lint.** `cargo lint-x86` and `cargo lint-arm` check the
  Linux code from macOS, and the workspace picks pure-Rust dependencies to
  keep that working (graviola over ring and aws-lc). rusqlite's bundled
  SQLite compiles C in its build script, which fails there with `failed to
  find tool "x86_64-linux-musl-gcc"`. `zig cc` behind a wrapper script fixes
  it, at the cost of another tool on every machine and runner. Turso, the
  pure-Rust SQLite rewrite, is pre-1.0 and still compiles C (simsimd).
- **Small and single-writer.** Tens to hundreds of records, written by the
  daemon alone. Every commit must survive a crash or power cut, since #11's
  crash-replay test restarts the daemon mid-change. One JSON file replaced
  by a rename would hold that much, but it rewrites everything on every
  change, and leaves transactions and the open lock to us.
- **A usual record shape.** Most resources are a document the daemon
  validates as a whole, and status it updates far more often than the
  document.
- **Tested on macOS.** Store logic is pure, so it is unit-tested on the
  development machine like the rest of the portable code.

## Decision

**The store is redb, behind a typed data access layer.** redb is a pure-Rust
embedded key-value store: copy-on-write B+trees with XXH3-checksummed pages,
MVCC readers alongside a single writer, and a commit that is one `fsync` and
a one-byte flip between two header slots. A crash leaves the last commit or
the one before it, never a mix.

### Opening

- The daemon opens `/var/lib/dassie/dassie.redb` in redb's default
  exclusive-writer mode, and every commit uses `Durability::Immediate`.
- A second daemon fails at open, with `Error::AlreadyRunning`. That is the
  single-instance lock #11 asks for.
- `Store::open` refuses a missing or empty file. Only `Store::init` creates a
  store, so an unmounted `/var/lib/dassie` or a lost file stops the daemon
  instead of reconciling to "nothing should run".
- `open` runs `check` (below) and refuses to start on any inconsistency.
- The CLI never opens the file while the daemon runs. It asks the daemon over
  its socket.

### The API

Only `dassie::store` names redb. Its table definitions are private, so no
caller can touch a table:

```rust
impl Store {
    pub fn init(path: &Path) -> Result<Store>;
    pub fn open(path: &Path) -> Result<Store>;   // migrates, then checks
    pub fn in_memory() -> Result<Store>;         // tests
    pub fn read<T, E: From<Error>>(&self, f: impl FnOnce(&ReadTxn) -> Result<T, E>) -> Result<T, E>;
    pub fn write<T, E: From<Error>>(&self, f: impl FnOnce(&mut WriteTxn) -> Result<T, E>) -> Result<T, E>;
}
```

- `write` commits when the closure returns `Ok`. Any `Err` aborts the whole
  write, whichever call returned it, so a write never commits half a change.
  A caller that wants to try one thing and fall back to another reads first,
  in the same transaction.
- The closure's error type is the caller's, so a request or reconcile error
  needs no store variant.
- `ReadTxn` and `WriteTxn` both implement a sealed `Reads` trait, which holds
  every read. Helpers take `&impl Reads`, so they work inside a write too.
- Writes are typed methods on `WriteTxn`, one per operation, such as
  `create_app`, `replace_app_spec` and `set_app_status`. There is no
  upsert.
- The API is sync, like vm-run (ADR 0012). Daemon threads queue on the
  writer, and reads run in parallel against a snapshot. The closures keep
  read transactions short, which matters because a live reader stops redb
  from reusing the pages it can see.
- Calling `write` from inside a write closure would deadlock on redb's
  single writer, so the store detects it and panics with a clear message.

**Closures do no I/O outside the store.** A systemd or Caddy call inside a
write would hold the only writer, and could outlive an aborted transaction.
Reconcile works in separate writes instead: record the intent, act, then
record the result.

### Errors

- **Typed errors**, such as `NotFound`, `AlreadyExists`,
  `Conflict { field, value, holder }`, `StillReferenced` and `Stale`, are for
  the caller to handle.
- **Storage and corruption errors are fatal.** These are any redb error, and
  a row that will not decode, which is reported with its table and key. After
  any I/O error, redb fails every later operation until the database is
  reopened. So the daemon exits and systemd restarts it, which is the path
  #11's crash-replay test already exercises.

### Resources

A resource kind is, by default, two tables sharing one primary key:

| Table | Key | Value |
| --- | --- | --- |
| `<kind>` | primary key | spec |
| `<kind>_status` | primary key | status |

- The **spec** is what was asked for. The operator sets it through the CLI,
  or the daemon sets it once when it creates a resource.
- The **status** is what the daemon has decided or done since. It is a
  separate table, so a status update never rewrites the spec and a spec
  update never overwrites status.
- `create_<kind>` takes both, and the delete removes both, so the API cannot
  leave one without the other.
- Status never copies what systemd already knows. Whether a unit is active,
  and its exit status, are read from systemd (ADR 0012).
- This is a default, not a framework. A kind that does not fit, such as an
  append-only log keyed by sequence, gets the tables it needs. No shared
  trait or macro is written until two kinds really share it.
- The exact tables and fields are left to the data model's ADR.

### Keys and values

**Keys are native redb types.** They are `&str`, unsigned integers or tuples
of them, so they sort meaningfully and range scans work: `(app, n)` lists one
app's records in order. Keys never contain `Option`, whose encoding redb's
next major version changes.

**Values are JSON.** Specs and statuses are Rust structs, serialised with
`serde_json`. Each value is stored as `(u64, &[u8])`: the row's revision
(below), then the JSON, so the Rust types never carry the revision.

- Every struct has `#[serde(deny_unknown_fields)]`. Optional fields are
  `#[serde(default)]` and skipped when unset. `#[serde(flatten)]` is not
  used, since it does not work with `deny_unknown_fields`.
- JSON over a positional format such as postcard: rows can be read in a
  dump, a field's name protects it from reordering, and `serde_json` is
  already a dependency. At this size the cost is nothing.
- The store decodes rows itself. redb's `Value::from_bytes` cannot fail, so
  JSON types get no redb `Value` impl, which could only panic.

### Revisions and conditional writes

**One revision counter for the whole store,** as etcd's `mod_revision` and
Kubernetes' `resourceVersion` work:

- `meta` holds the store's revision. A write that changes anything takes the
  next one, once for the whole transaction, and stamps it on every row it
  writes. A write that changes nothing leaves it alone.
- Reads return `Versioned<T> { revision, value }`.
- A recreated key gets a fresh, higher revision. So a caller holding the
  revision of a row that has since been deleted can never match the new one,
  which a per-row counter starting at 1 would allow.
- Revisions order every change in the store, and "has anything changed since
  R?" is one read of `meta`.

**Every write method takes a `Precondition`:** `Any`, `Absent` or
`At(revision)`. A row that does not match returns
`Stale { key, expected, actual }`. Like any `Err`, it aborts the whole write,
so a write over several rows applies all of them or none.

- **Spec and status get revisions of their own,** because they are separate
  tables. A status write never moves the spec's revision, so the spec's
  revision acts as Kubernetes' `generation`. A status can record the spec
  revision it acted on, as `observedGeneration` does.
- **Reconcile's last step is conditional.** It reads a spec at revision R,
  records its intent, acts outside the store, then records the result only
  if the spec is still at R. If the operator changed the spec meanwhile, the
  stale result is refused rather than written over the change.
- **The CLI** can apply unconditionally. A read-edit-write command sends back
  the revision it read.

### Lookups scan

There are no stored index tables. A lookup by anything other than the primary
key is a scan, and so are the uniqueness and reference checks a write makes,
inside the writing transaction. At hundreds of records a scan takes
microseconds.

- This leaves out a whole class of corruption, an index that has drifted
  from its records, and the code that would maintain and check it.
- The API hides how a lookup works. A stored index added later, for a
  measured need, is a change inside the store plus a migration that builds
  it.

**Allocations are records.** A host-wide value, such as ADR 0008's index and
with it ADR 0013's uid, is held by an allocation record. That record names
its holder, and it outlives the resource that used it:

- reconcile releases it only once it has confirmed teardown: the unit
  stopped, and the TAP and chroot gone;
- a new allocation takes the lowest value not held, found in the write that
  takes it.

So a value is never reused while something on the host still carries it.

### Check

`Store::check` runs in one read transaction. It:

- decodes every row;
- pairs every spec with its status;
- checks that every reference names a record that exists;
- checks that every allocation's holder exists or is being torn down;
- checks that no unique field is held twice;
- checks that no row's revision is above the store's.

It returns every inconsistency it finds, each with its table, key and what
was expected, rather than stopping at the first. `open` runs it, tests run it
after every operation, and the CLI exposes it as a debug command.

### Schema changes

- **The schema version** lives in a `meta` table.
- **When it bumps:** any change to what can be serialised bumps it, such as a
  new field or a new enum variant, even when no row needs rewriting. That
  bump comes with an empty migration. An older binary would refuse the new
  rows, so it must refuse the store at open instead.
- **Enforced in tests:** a fully populated sample of every type is checked in
  as JSON under the current version. A change in shape without a bump fails
  the tests.
- **Migrations** run in `Store::open`, in order, each in one write
  transaction with its bump. They work on `serde_json::Value`, not the
  current structs, which reject rows whose fields have since been renamed
  or removed.
- **A copy first:** before migrating, `open` copies the file to
  `dassie.redb.v<N>`. A store newer than the binary is refused, so rolling
  the binary back past a bump means restoring that copy.

### Dump and load

`dassie store dump` and `dassie store load` run with the daemon stopped,
which the exclusive open enforces. A dump writes every table as JSON, and a
row that will not decode as its raw bytes, so it works on exactly the store
the daemon refused. One tool covers four needs:

- inspecting the store;
- fixing a bad row by hand;
- backups;
- redb's major versions, which have changed the file format before: dump
  with the old binary, load with the new one, one major at a time.

While the daemon runs, it writes the same export from a read transaction on
request, since copying the live file is not safe.

The dependency is `redb = "4.6"`: any 4.x from 4.6 on, the release that
stops a damaged file from being opened as an empty one. A major bump is a
deliberate change, and comes with a test that loads a dump made by the
previous version.

### Testing

redb is light enough to give every test its own store.

- **Unit tests** use `Store::in_memory()`, on redb's in-memory backend, with
  no files and no shared fixtures. A fresh store with ten commits takes
  about 5 ms in a debug build on the development Mac, and a commit about
  0.5 ms.
- **Per operation:** the effect on records, each typed error, and an aborted
  write leaving no trace.
- **Model tests** run seeded random sequences of operations against both the
  store and a plain `BTreeMap` model, including conditional writes with
  stale and current revisions. They compare every result, error and
  revision, and run `check` after each step. A failure prints its seed. This catches
  logic the store and `check` could both get wrong in the same way.
- **Fault injection:** a test backend wraps the in-memory one and fails the
  Nth write or sync. With it, the abort and fatal-error paths are actually
  run.
- **On disk,** with `tempfile`, a few tests check that:
  - reopening sees every commit;
  - `open` refuses a missing file;
  - a second open fails with `AlreadyRunning`;
  - migrations upgrade a fixture from each earlier version, after making
    their copy.

  File-backed commits take about 4–5 ms on the Mac, so these tests stay
  few.
- The store's callers, such as the reconcile loop, test against a real
  in-memory store rather than a mock.

All of it runs on macOS.

## Consequences

- The workspace stays pure Rust, and the lint aliases work as they do today.
  redb, serde and serde_json come to about 16 crates.
- Commits are atomic and durable without the rename and `sync` dance
  `deploy-image.sh` does today.
- There is no SQL. Uniqueness, references and validation are Rust in the
  store, and `check` plus the model tests are what keep them honest.
- There is no `sqlite3` shell. Inspection is `dassie store dump` with the
  daemon stopped, or an export from the running daemon.
- Lookups scan. That is fine at this scale, and a store that grows past it
  can add stored indexes without changing callers.
- Conditional writes make read-act-write safe across separate transactions,
  which reconcile's "intent, act, result" depends on. They also leave room
  for a change feed or watch later, keyed by revision.
- A daemon cannot be rolled back past a schema bump except by restoring the
  copy `open` made.
- An I/O error restarts the daemon rather than being handled in place.
- An allocated value stays unusable until its teardown is confirmed, so a
  host with stuck teardowns can run out sooner.
- This replaces SQLite wherever #11, ADR 0002 and ADR 0008 mention it. Those
  ADRs' decisions otherwise stand.
