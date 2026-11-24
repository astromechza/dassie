//! The daemon's store: redb behind a typed data access layer (ADR 0016).
//!
//! Nothing outside this module names redb or opens a table. Callers get
//! [`Store::read`] and [`Store::write`], and typed methods on the
//! transactions those hand to their closures. Each resource kind adds its
//! own methods and its entry in [`KINDS`]; this file is the plumbing they
//! share: the `meta` table, revisions and preconditions, and the rule that a
//! failed store method fails the whole write.

use std::cell::Cell;
use std::fs;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

mod check;
mod error;
#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod widget;

pub(crate) use check::KindDef;
pub(crate) use error::{Error, Inconsistency};

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// The schema version this binary reads and writes.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// A table of rows: key, then the row's revision number and its JSON.
pub(crate) type Rows = TableDefinition<'static, &'static str, (u64, &'static [u8])>;

/// The store's own counters: schema version, revision and epoch.
const META: TableDefinition<'static, &'static str, u64> = TableDefinition::new("meta");
const META_VERSION: &str = "schema_version";
const META_REVISION: &str = "revision";
const META_EPOCH: &str = "epoch";

/// Every resource kind the store holds, for `init` and `check`.
const KINDS: &[KindDef] = &[
    #[cfg(test)]
    widget::KIND,
];

/// Migrations from version N to N+1 are `MIGRATIONS[N - 1]`. They work on
/// raw rows (`serde_json::Value`), never on the current structs.
type Migration = fn(&redb::WriteTransaction) -> Result<()>;
const MIGRATIONS: &[Migration] = &[];
const _: () = assert!(MIGRATIONS.len() as u64 + 1 == SCHEMA_VERSION);

/// One point in the store's history. The epoch changes whenever the store's
/// history does (`init`, restore, load), so a revision from before a
/// restore never matches one from after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Revision {
    pub(crate) epoch: u64,
    pub(crate) n: u64,
}

/// A value and the revision of the write that last stored it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Versioned<T> {
    pub(crate) revision: Revision,
    pub(crate) value: T,
}

/// What a write requires of a row before it changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Precondition {
    /// Whatever the row holds.
    Any,
    /// The row must not exist.
    Absent,
    /// The row must be at exactly this revision.
    At(Revision),
}

/// The store. One per process: a second open of the same file fails with
/// [`Error::AlreadyRunning`].
#[derive(Debug)]
pub(crate) struct Store {
    db: Database,
    epoch: u64,
}

impl Store {
    /// Creates a new, empty store at `path`, which must not exist yet.
    pub(crate) fn init(path: &Path) -> Result<Store> {
        if path.exists() {
            return Err(Error::StoreExists(path.to_owned()));
        }
        Store::bootstrap(Database::builder().create(path)?)
    }

    /// Opens the store at `path`: migrates it if it is older than this
    /// binary, then refuses it if `check` finds anything.
    pub(crate) fn open(path: &Path) -> Result<Store> {
        match fs::metadata(path) {
            Ok(m) if m.len() > 0 => {}
            _ => return Err(Error::NoStore(path.to_owned())),
        }
        let db = match Database::builder().open(path) {
            Ok(db) => db,
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => return Err(Error::AlreadyRunning),
            Err(e) => return Err(e.into()),
        };
        let version = read_meta(&db.begin_read()?, META_VERSION)?;
        if version > SCHEMA_VERSION {
            return Err(Error::TooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < SCHEMA_VERSION {
            // Keep the store as it was, for rolling the binary back. No
            // transaction is open, so the file is consistent on disk.
            let copy = format!("{}.v{version}", path.display());
            fs::copy(path, &copy).map_err(|e| Error::Io(std::sync::Arc::new(e)))?;
            for (from, migrate) in MIGRATIONS
                .iter()
                .enumerate()
                .skip(usize::try_from(version - 1).expect("schema versions fit in usize"))
            {
                let tx = db.begin_write()?;
                migrate(&tx)?;
                tx.open_table(META)?.insert(META_VERSION, from as u64 + 2)?;
                tx.commit()?;
            }
        }
        let store = Store::from_db(db)?;
        let found = store.check()?;
        if !found.is_empty() {
            return Err(Error::Inconsistent(found));
        }
        Ok(store)
    }

    /// A new, empty store in memory, for tests.
    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Store> {
        Store::with_backend(redb::backends::InMemoryBackend::new())
    }

    /// A new, empty store on any backend, for tests that inject faults.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: impl redb::StorageBackend) -> Result<Store> {
        Store::bootstrap(Database::builder().create_with_backend(backend)?)
    }

    /// Reopens a store a test made on `backend`.
    #[cfg(test)]
    pub(crate) fn reopen_backend(backend: impl redb::StorageBackend) -> Result<Store> {
        Store::from_db(Database::builder().create_with_backend(backend)?)
    }

    /// Writes the meta rows and creates every table.
    fn bootstrap(db: Database) -> Result<Store> {
        let tx = db.begin_write()?;
        {
            let mut meta = tx.open_table(META)?;
            meta.insert(META_VERSION, SCHEMA_VERSION)?;
            meta.insert(META_REVISION, 0)?;
            meta.insert(META_EPOCH, fresh_epoch())?;
        }
        for kind in KINDS {
            tx.open_table(kind.spec)?;
            tx.open_table(kind.status)?;
        }
        tx.commit()?;
        Store::from_db(db)
    }

    fn from_db(db: Database) -> Result<Store> {
        let epoch = read_meta(&db.begin_read()?, META_EPOCH)?;
        Ok(Store { db, epoch })
    }

    /// Runs `f` against a snapshot. Keep it short: a live reader stops redb
    /// reusing the pages it can see.
    pub(crate) fn read<T, E: From<Error>>(
        &self,
        f: impl FnOnce(&ReadTxn) -> Result<T, E>,
    ) -> Result<T, E> {
        let txn = ReadTxn {
            tx: self.db.begin_read().map_err(Error::from)?,
            epoch: self.epoch,
        };
        f(&txn)
    }

    /// Runs `f` in a write transaction and commits it if `f` returns `Ok`
    /// and no store method in it failed. Otherwise nothing is written: the
    /// first store error, or `f`'s own error, is returned.
    ///
    /// `f` must do no I/O outside the store (ADR 0016).
    ///
    /// # Panics
    ///
    /// If called from inside another `write` on this thread, which would
    /// otherwise deadlock on redb's single writer.
    pub(crate) fn write<T, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut WriteTxn) -> Result<T, E>,
    ) -> Result<T, E> {
        let _nested = NestGuard::enter();
        let mut txn = WriteTxn::begin(&self.db, self.epoch)?;
        let out = f(&mut txn);
        match (txn.failed.take(), out) {
            (Some(e), _) => Err(e.into()),
            (None, Err(e)) => Err(e),
            (None, Ok(v)) => {
                txn.commit()?;
                Ok(v)
            }
        }
    }

    /// Checks everything an SQL engine would have, and returns every
    /// problem it finds.
    pub(crate) fn check(&self) -> Result<Vec<Inconsistency>> {
        self.read(check::run)
    }
}

fn read_meta(tx: &redb::ReadTransaction, key: &str) -> Result<u64> {
    tx.open_table(META)?
        .get(key)?
        .map(|v| v.value())
        .ok_or_else(|| Error::Corrupt {
            table: "meta".into(),
            key: key.into(),
            reason: "missing".into(),
        })
}

/// A random id for a new history of the store.
fn fresh_epoch() -> u64 {
    let mut h = RandomState::new().build_hasher();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    h.write_u128(now.as_nanos());
    h.write_u32(std::process::id());
    h.finish()
}

thread_local! {
    static IN_WRITE: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread as inside a `write` closure.
struct NestGuard;

impl NestGuard {
    fn enter() -> NestGuard {
        assert!(
            !IN_WRITE.replace(true),
            "Store::write called inside another write: redb has one writer, so this would deadlock"
        );
        NestGuard
    }
}

impl Drop for NestGuard {
    fn drop(&mut self) {
        IN_WRITE.set(false);
    }
}

mod sealed {
    pub(crate) trait Sealed {}
}

/// Either transaction, for reads that work in both.
#[derive(Clone, Copy)]
pub(crate) enum Txn<'a> {
    Read(&'a redb::ReadTransaction),
    Write(&'a redb::WriteTransaction),
}

/// Everything that reads the store. `ReadTxn` and `WriteTxn` both implement
/// it, so a helper taking `&impl Reads` works inside a write too, and sees
/// that write's own changes.
pub(crate) trait Reads: sealed::Sealed {
    #[doc(hidden)]
    fn txn(&self) -> Txn<'_>;

    /// The store's current history.
    fn epoch(&self) -> u64;

    /// The revision of the last committed write.
    fn revision(&self) -> Result<Revision> {
        let n = match self.txn() {
            Txn::Read(t) => t.open_table(META)?.get(META_REVISION)?.map(|v| v.value()),
            Txn::Write(t) => t.open_table(META)?.get(META_REVISION)?.map(|v| v.value()),
        };
        Ok(Revision {
            epoch: self.epoch(),
            n: n.unwrap_or_default(),
        })
    }
}

/// A read-only snapshot.
pub(crate) struct ReadTxn {
    tx: redb::ReadTransaction,
    epoch: u64,
}

impl sealed::Sealed for ReadTxn {}

impl Reads for ReadTxn {
    fn txn(&self) -> Txn<'_> {
        Txn::Read(&self.tx)
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// A write in progress. Its methods are the only way to change the store.
pub(crate) struct WriteTxn {
    tx: redb::WriteTransaction,
    epoch: u64,
    /// The store's revision when this write began.
    base: u64,
    changed: bool,
    /// The first error a store method returned. It fails the whole write.
    failed: Option<Error>,
}

impl sealed::Sealed for WriteTxn {}

impl Reads for WriteTxn {
    fn txn(&self) -> Txn<'_> {
        Txn::Write(&self.tx)
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl WriteTxn {
    fn begin(db: &Database, epoch: u64) -> Result<WriteTxn> {
        let tx = db.begin_write()?;
        let base = tx
            .open_table(META)?
            .get(META_REVISION)?
            .map_or(0, |v| v.value());
        Ok(WriteTxn {
            tx,
            epoch,
            base,
            changed: false,
            failed: None,
        })
    }

    fn commit(self) -> Result<()> {
        if self.changed {
            self.tx
                .open_table(META)?
                .insert(META_REVISION, self.base + 1)?;
        }
        self.tx.commit()?;
        Ok(())
    }

    /// The revision every row this write stores is stamped with.
    fn next(&self) -> Revision {
        Revision {
            epoch: self.epoch,
            n: self.base + 1,
        }
    }

    /// Runs one store method. An error, or an earlier one, fails the write.
    fn op<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        let out = f(self);
        if let Err(e) = &out {
            self.failed = Some(e.clone());
        }
        out
    }

    /// Fails with `Stale` unless the row matches `pre`.
    fn require(&self, table: Rows, key: &str, pre: Precondition) -> Result<()> {
        let actual = row_revision(self, table, key)?;
        let ok = match pre {
            Precondition::Any => true,
            Precondition::Absent => actual.is_none(),
            Precondition::At(want) => actual == Some(want),
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Stale {
                table: table_name(table),
                key: key.into(),
                expected: pre,
                actual,
            })
        }
    }

    /// Stores `value` under `key`, stamped with this write's revision.
    fn put<T: Serialize>(&mut self, table: Rows, key: &str, value: &T) -> Result<Revision> {
        let json = serde_json::to_vec(value).map_err(|e| Error::Corrupt {
            table: table_name(table),
            key: key.into(),
            reason: format!("encode: {e}"),
        })?;
        let rev = self.next();
        self.tx
            .open_table(table)?
            .insert(key, (rev.n, json.as_slice()))?;
        self.changed = true;
        Ok(rev)
    }

    /// Removes `key`. Returns whether it was there.
    fn remove(&mut self, table: Rows, key: &str) -> Result<bool> {
        let gone = self.tx.open_table(table)?.remove(key)?.is_some();
        self.changed |= gone;
        Ok(gone)
    }
}

pub(crate) fn table_name(table: Rows) -> String {
    redb::TableHandle::name(&table).to_owned()
}

/// A raw row: its revision number and its JSON.
type RawRow = (u64, Vec<u8>);

fn raw_get(r: &impl Reads, table: Rows, key: &str) -> Result<Option<RawRow>> {
    fn from(
        t: &impl ReadableTable<&'static str, (u64, &'static [u8])>,
        key: &str,
    ) -> Result<Option<RawRow>> {
        Ok(t.get(key)?.map(|g| {
            let (n, json) = g.value();
            (n, json.to_vec())
        }))
    }
    match r.txn() {
        Txn::Read(t) => from(&t.open_table(table)?, key),
        Txn::Write(t) => from(&t.open_table(table)?, key),
    }
}

fn raw_scan(r: &impl Reads, table: Rows) -> Result<Vec<(String, RawRow)>> {
    fn from(
        t: &impl ReadableTable<&'static str, (u64, &'static [u8])>,
    ) -> Result<Vec<(String, RawRow)>> {
        let mut rows = Vec::new();
        for entry in t.iter()? {
            let (k, v) = entry?;
            let (n, json) = v.value();
            rows.push((k.value().to_owned(), (n, json.to_vec())));
        }
        Ok(rows)
    }
    match r.txn() {
        Txn::Read(t) => from(&t.open_table(table)?),
        Txn::Write(t) => from(&t.open_table(table)?),
    }
}

fn row_revision(r: &impl Reads, table: Rows, key: &str) -> Result<Option<Revision>> {
    Ok(raw_get(r, table, key)?.map(|(n, _)| Revision {
        epoch: r.epoch(),
        n,
    }))
}

fn decode<T: DeserializeOwned>(
    r: &impl Reads,
    table: Rows,
    key: &str,
    (n, json): RawRow,
) -> Result<Versioned<T>> {
    let value = serde_json::from_slice(&json).map_err(|e| Error::Corrupt {
        table: table_name(table),
        key: key.into(),
        reason: e.to_string(),
    })?;
    Ok(Versioned {
        revision: Revision {
            epoch: r.epoch(),
            n,
        },
        value,
    })
}

/// The row at `key`, decoded.
pub(crate) fn get<T: DeserializeOwned>(
    r: &impl Reads,
    table: Rows,
    key: &str,
) -> Result<Option<Versioned<T>>> {
    raw_get(r, table, key)?
        .map(|row| decode(r, table, key, row))
        .transpose()
}

/// Every row in `table`, in key order, decoded.
pub(crate) fn scan<T: DeserializeOwned>(
    r: &impl Reads,
    table: Rows,
) -> Result<Vec<(String, Versioned<T>)>> {
    raw_scan(r, table)?
        .into_iter()
        .map(|(key, row)| {
            let v = decode(r, table, &key, row)?;
            Ok((key, v))
        })
        .collect()
}
