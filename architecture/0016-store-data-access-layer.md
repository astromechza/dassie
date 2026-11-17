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
- **Small and single-writer.** Tens to hundreds of rows, written by the
  daemon alone. Every commit must survive a crash or power cut, since #11's
  crash-replay test restarts the daemon mid-change.
- **One row shape.** Each resource is a document the daemon validates as a
  whole, a few fields it looks rows up by, and status it updates far more
  often than the document.
- **Tested on macOS.** Store logic is pure, so it is unit-tested on the
  development machine like the rest of the portable code.

## Decision

**The store is redb, behind a typed data access layer.** redb is a pure-Rust
embedded key-value store: copy-on-write B+trees with XXH3-checksummed pages,
MVCC readers alongside a single writer, and a commit that is one `fsync` and
a one-byte flip between two header slots. A crash leaves the last commit or
the one before it, never a mix.

**One process opens it.** The daemon opens `/var/lib/dassie/dassie.redb` in
redb's default exclusive-writer mode, so a second daemon fails at open. That
is the single-instance lock #11 asks for. Every commit uses
`Durability::Immediate`. The CLI never opens the file; it asks the daemon
over its socket.

**Only `dassie::store` names redb.** The module's public API is typed, and
its table definitions are private, so no caller can write a row without its
indexes:

```rust
impl Store {
    pub fn open(path: &Path) -> Result<Store>;   // runs migrations
    pub fn read<T>(&self, f: impl FnOnce(&Read) -> Result<T>) -> Result<T>;
    pub fn write<T>(&self, f: impl FnOnce(&mut Write) -> Result<T>) -> Result<T>;
}
```

`write` commits when the closure returns `Ok` and aborts on `Err`, so a
failed check halfway through a change leaves nothing behind. `Read` and
`Write` wrap redb's transactions and carry one typed method per operation,
such as `put_app` or `instances_of`. `Write` also has every read, so a check
and the change it guards share a transaction. One logical change, such as
one CLI request or one reconcile step, is one write. The API is sync, like
vm-run (ADR 0012): daemon threads queue on the writer, and reads run in
parallel against a snapshot. The closures keep read transactions short,
which matters because a live reader stops redb from reusing the pages it can
see.

**Each resource kind is a set of tables sharing one primary key:**

| Table | Key | Value |
| --- | --- | --- |
| `<kind>` | primary key | spec |
| `<kind>_status` | primary key | status |
| `<kind>_by_<field>` | index key | primary key |

- The **spec** is what was asked for. The operator sets it through the CLI,
  or the daemon sets it once when it creates a resource.
- The **status** is what the daemon has decided or done since. It is a
  separate table, so a status update never rewrites the spec and a spec
  update never overwrites status. The two rows are created and deleted in
  the same transaction, and one without the other is corruption.
- Status never copies what systemd already knows. Whether a unit is active,
  and its exit status, are read from systemd (ADR 0012).
- The exact tables, fields and indexes of each resource are left to the data
  model's ADR.

**Keys are native redb types; values are JSON.** Keys are `&str`, unsigned
integers or tuples of them, so they sort meaningfully and range scans work:
`(app, n)` lists one app's rows in order. Specs and statuses are Rust
structs, serialised with `serde_json` into `&[u8]` values.

- Every struct has `#[serde(deny_unknown_fields)]`, and optional fields are
  `#[serde(default)]` and skipped when unset. A new optional field then
  needs no migration, and a misspelt one is an error.
- JSON over a positional format such as postcard: rows can be read in a
  dump, a field's name protects it from reordering, and `serde_json` is
  already a dependency. At this size the cost is nothing.
- Decoding happens in the store, which turns a bad row into an error naming
  its table and key. redb's `Value::from_bytes` cannot fail, so JSON types
  are not given a redb `Value` impl, which could only panic.

**Secondary indexes are maintained by the store, in the writing
transaction.**

- A unique index is a table from index key to primary key. A non-unique one
  is a multimap table from index key to primary keys.
- Each resource has one pure function that returns the index keys of a
  record. Unit tests can call it directly, and `check` uses it too.
- A write loads the old record, computes its index keys and the new
  record's, and removes those that went away. It then checks each new
  unique key, adds the new entries, and writes the record. A key held by
  another record returns a typed error, and the write aborts, undoing the
  removals with it.
- A delete removes the record, its status and the index entries computed
  from the removed record.
- Allocations are unique indexes keyed by the allocated value. The lowest
  free value is the first gap in that table's keys, found inside the write
  that takes it, so two allocations cannot collide.
- References between resources are checked the same way. For example,
  deleting a record that still has children fails while the children's
  index lists any.

**`Store::check` verifies everything an SQL engine would have.** It decodes
every row, pairs every spec with its status, rebuilds every index from the
records, and diffs the result against the stored index tables. It runs in a
read transaction. Tests call it after every operation, and the CLI exposes
it later as a debug command.

**Migrations.** A `meta` table holds the schema version. `Store::open` applies
each newer migration in order, each in one write transaction with its
version bump. A migration is Rust that may rewrite values and rebuild
indexes. A store newer than the binary is refused, not opened. Adding an
index means a migration that builds it from the existing records.

**Testing.** redb is light enough to give every test its own database.

- **Unit tests** open a fresh store on `redb::backends::InMemoryBackend`, with
  no files and no shared fixtures. A fresh store with ten commits takes
  about 5 ms in a debug build on the development Mac, and a commit about
  0.5 ms.
- **Per operation:** the effect on records and statuses, index
  conflicts, an update moving an index entry, a delete removing its entries,
  and a failed write leaving no trace.
- **Sequences:** randomised runs of operations against one store, with
  `check` after each step, catch any path that forgets an index.
- **On disk,** with `tempfile`: reopening sees every commit, migrations
  upgrade a stored fixture from each earlier version, and a second open of a
  live file fails. File-backed commits with `Immediate` durability take about
  4–5 ms on the Mac, so these tests stay few.
- The store's callers, such as the reconcile loop, test against a real
  in-memory store rather than a mock.

All of it runs on macOS.

## Consequences

- The workspace stays pure Rust, and the lint aliases work as they do today.
  redb, serde and serde_json come to about 16 crates.
- Commits are atomic and durable without the rename and `sync` dance
  `deploy-image.sh` does today.
- There is no SQL. Uniqueness, references and validation are Rust in the
  store, and `check` plus the sequence tests are what keep them honest.
  Every new filter is an index table and a migration.
- There is no `sqlite3` shell. Inspecting a live store needs a CLI dump
  through the daemon, or the daemon stopped. redb's single-writer mode
  could later let a read-only handle open the file while the daemon runs.
- Debugging data is readable JSON, keyed by readable keys.
- This replaces SQLite wherever #11, ADR 0002 and ADR 0008 mention it. Those
  ADRs' decisions otherwise stand.
- redb is pinned to its current major version. A major bump can change the
  file format, so it comes with an upgrade test against a stored fixture.
