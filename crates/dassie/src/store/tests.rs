//! Store tests: one fresh in-memory store each, plus a few on disk.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::StorageBackend;
use redb::backends::InMemoryBackend;

use super::widget::{Colour, Phase, SPEC, STATUS, WidgetReads, WidgetSpec, WidgetStatus};
use super::*;

fn spec(size: u32, label: Option<&str>) -> WidgetSpec {
    WidgetSpec {
        size,
        colour: Colour::Red,
        label: label.map(Into::into),
    }
}

fn pending() -> WidgetStatus {
    WidgetStatus {
        phase: Phase::Pending,
        observed: None,
    }
}

fn create(store: &Store, key: &str, label: Option<&str>) -> Revision {
    store
        .write(|w| w.create_widget(key, &spec(1, label), &pending()))
        .unwrap()
}

fn assert_consistent(store: &Store) {
    assert_eq!(store.check().unwrap(), Vec::new());
}

#[test]
fn a_write_stamps_one_revision_on_every_row() {
    let store = Store::in_memory().unwrap();
    let rev = create(&store, "a", None);
    let w = store.read(|r| r.widget("a")).unwrap().unwrap();
    assert_eq!(rev.n, 1);
    assert_eq!(w.spec.revision, rev);
    assert_eq!(w.status.revision, rev);
    assert_eq!(store.read(Reads::revision).unwrap(), rev);
    assert_consistent(&store);
}

#[test]
fn a_status_write_never_moves_the_spec_revision() {
    let store = Store::in_memory().unwrap();
    let rev = create(&store, "a", None);
    let ready = WidgetStatus {
        phase: Phase::Ready,
        observed: Some(rev.n),
    };
    let status_rev = store
        .write(|w| w.set_widget_status("a", &ready, Precondition::At(rev), Precondition::At(rev)))
        .unwrap();
    let w = store.read(|r| r.widget("a")).unwrap().unwrap();
    assert_eq!(w.spec.revision, rev);
    assert_eq!(w.status.revision, status_rev);
    assert!(status_rev.n > rev.n);
}

#[test]
fn a_write_that_changes_nothing_takes_no_revision() {
    let store = Store::in_memory().unwrap();
    let rev = create(&store, "a", None);
    store.write(|r| r.widgets().map(drop)).unwrap();
    assert_eq!(store.read(Reads::revision).unwrap(), rev);
}

#[test]
fn typed_errors() {
    let store = Store::in_memory().unwrap();
    let rev = create(&store, "a", Some("x"));
    let err = store
        .write(|w| w.create_widget("a", &spec(2, None), &pending()))
        .unwrap_err();
    assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
    let err = store
        .write(|w| w.create_widget("b", &spec(2, Some("x")), &pending()))
        .unwrap_err();
    assert!(
        matches!(&err, Error::Conflict { holder, .. } if holder == "a"),
        "{err}"
    );
    let err = store
        .write(|w| w.replace_widget_spec("nope", &spec(2, None), Precondition::Any))
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
    assert!(!err.is_fatal());
    // A widget may keep its own label.
    store
        .write(|w| w.replace_widget_spec("a", &spec(3, Some("x")), Precondition::At(rev)))
        .unwrap();
}

#[test]
fn a_stale_precondition_changes_nothing() {
    let store = Store::in_memory().unwrap();
    let old = create(&store, "a", None);
    store
        .write(|w| w.replace_widget_spec("a", &spec(2, None), Precondition::At(old)))
        .unwrap();
    let err = store
        .write(|w| w.replace_widget_spec("a", &spec(3, None), Precondition::At(old)))
        .unwrap_err();
    assert!(
        matches!(&err, Error::Stale { actual: Some(a), .. } if a.n == 2),
        "{err}"
    );
    let w = store.read(|r| r.widget("a")).unwrap().unwrap();
    assert_eq!(w.spec.value.size, 2);
}

#[test]
fn a_status_write_against_a_changed_spec_is_stale() {
    let store = Store::in_memory().unwrap();
    let acted_on = create(&store, "a", None);
    let status = store
        .read(|r| r.widget("a"))
        .unwrap()
        .unwrap()
        .status
        .revision;
    store
        .write(|w| w.replace_widget_spec("a", &spec(2, None), Precondition::Any))
        .unwrap();
    let ready = WidgetStatus {
        phase: Phase::Ready,
        observed: Some(acted_on.n),
    };
    let err = store
        .write(|w| {
            w.set_widget_status(
                "a",
                &ready,
                Precondition::At(status),
                Precondition::At(acted_on),
            )
        })
        .unwrap_err();
    assert!(
        matches!(&err, Error::Stale { table, .. } if table == "widget"),
        "{err}"
    );
}

#[test]
fn a_recreated_key_never_matches_an_old_revision() {
    let store = Store::in_memory().unwrap();
    let first = create(&store, "a", None);
    store
        .write(|w| w.delete_widget("a", Precondition::At(first)))
        .unwrap();
    let second = create(&store, "a", None);
    assert!(second.n > first.n);
    let err = store
        .write(|w| w.replace_widget_spec("a", &spec(2, None), Precondition::At(first)))
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");
}

#[test]
fn a_revision_from_another_epoch_never_matches() {
    let one = Store::in_memory().unwrap();
    let two = Store::in_memory().unwrap();
    let rev = create(&one, "a", None);
    assert_eq!(create(&two, "a", None).n, rev.n);
    assert_ne!(one.epoch, two.epoch);
    let err = two
        .write(|w| w.replace_widget_spec("a", &spec(2, None), Precondition::At(rev)))
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");
}

#[test]
fn a_caught_error_still_fails_the_write() {
    let store = Store::in_memory().unwrap();
    let old = create(&store, "a", None);
    store
        .write(|w| w.replace_widget_spec("a", &spec(2, None), Precondition::Any))
        .unwrap();
    let err = store
        .write(|w| {
            w.create_widget("b", &spec(1, None), &pending())?;
            // Swallow the stale error and carry on: the write must not
            // commit "b" without this replace.
            drop(w.replace_widget_spec("a", &spec(9, None), Precondition::At(old)));
            Ok::<_, Error>(())
        })
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");
    assert_eq!(store.read(|r| r.widget("b")).unwrap(), None);
}

#[test]
fn after_a_failure_every_store_method_fails() {
    let store = Store::in_memory().unwrap();
    store
        .write(|w| {
            assert!(
                w.replace_widget_spec("nope", &spec(1, None), Precondition::Any)
                    .is_err()
            );
            let err = w
                .create_widget("b", &spec(1, None), &pending())
                .unwrap_err();
            assert!(matches!(err, Error::NotFound { .. }), "{err}");
            Ok::<_, Error>(())
        })
        .unwrap_err();
}

#[test]
fn a_caught_read_error_still_fails_the_write() {
    let store = Store::in_memory().unwrap();
    raw_put(
        &store,
        SPEC,
        "junk",
        1,
        br#"{"size":1,"colour":"red","extra":1}"#,
    );
    raw_put(&store, STATUS, "junk", 1, br#"{"phase":"pending"}"#);
    let err = store
        .write(|w| {
            w.create_widget("b", &spec(1, None), &pending())?;
            assert!(w.widget("junk").is_err());
            Ok::<_, Error>(())
        })
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    assert_eq!(store.read(|r| r.widget("b")).unwrap(), None);
}

#[test]
fn a_read_after_a_failure_returns_it() {
    let store = Store::in_memory().unwrap();
    create(&store, "a", None);
    store
        .write(|w| {
            assert!(
                w.replace_widget_spec("nope", &spec(1, None), Precondition::Any)
                    .is_err()
            );
            let err = w.widget("a").unwrap_err();
            assert!(matches!(err, Error::NotFound { .. }), "{err}");
            let err = w.revision().unwrap_err();
            assert!(matches!(err, Error::NotFound { .. }), "{err}");
            Ok::<_, Error>(())
        })
        .unwrap_err();
}

/// A caller's own error type works with `?` on store calls.
#[test]
fn closures_return_the_callers_error() {
    #[derive(Debug)]
    enum AppError {
        Store(Error),
        TooBig,
    }
    impl From<Error> for AppError {
        fn from(e: Error) -> Self {
            AppError::Store(e)
        }
    }
    let store = Store::in_memory().unwrap();
    let err = store
        .write(|w| -> Result<(), AppError> {
            w.create_widget("a", &spec(100, None), &pending())?;
            Err(AppError::TooBig)
        })
        .unwrap_err();
    assert!(matches!(err, AppError::TooBig));
    assert_eq!(store.read(|r| r.widget("a")).unwrap(), None);
    let err = store
        .write(|w| -> Result<(), AppError> {
            w.replace_widget_spec("nope", &spec(1, None), Precondition::Any)?;
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(err, AppError::Store(Error::NotFound { .. })));
}

#[test]
fn reads_inside_a_write_see_its_changes() {
    let store = Store::in_memory().unwrap();
    store
        .write(|w| {
            w.create_widget("a", &spec(1, None), &pending())?;
            assert!(w.widget("a")?.is_some());
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
#[should_panic(expected = "inside another write")]
fn a_nested_write_panics() {
    let store = Store::in_memory().unwrap();
    drop(store.write(|_| store.write(|_| Ok::<_, Error>(()))));
}

#[test]
fn a_write_after_a_panicking_one_still_works() {
    let store = Store::in_memory().unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(store.write(|w| -> Result<(), Error> {
            w.create_widget("a", &spec(1, None), &pending())?;
            panic!("boom");
        }));
    }));
    assert!(panicked.is_err());
    assert_eq!(store.read(|r| r.widget("a")).unwrap(), None);
    create(&store, "a", None);
}

/// Rows written behind the store's back, to give `check` something to find.
fn raw_put(store: &Store, table: Rows, key: &str, n: u64, json: &[u8]) {
    let tx = store.db.begin_write().unwrap();
    tx.open_table(table)
        .unwrap()
        .insert(key, (n, json))
        .unwrap();
    tx.commit().unwrap();
}

#[test]
fn check_reports_every_problem() {
    let store = Store::in_memory().unwrap();
    create(&store, "a", Some("x"));
    raw_put(&store, SPEC, "lonely", 1, br#"{"size":1,"colour":"red"}"#);
    raw_put(&store, STATUS, "orphan", 1, br#"{"phase":"pending"}"#);
    raw_put(&store, SPEC, "future", 99, br#"{"size":1,"colour":"red"}"#);
    raw_put(&store, STATUS, "future", 1, br#"{"phase":"pending"}"#);
    raw_put(
        &store,
        SPEC,
        "dup",
        1,
        br#"{"size":1,"colour":"red","label":"x"}"#,
    );
    raw_put(&store, STATUS, "dup", 1, br#"{"phase":"pending"}"#);
    raw_put(
        &store,
        SPEC,
        "junk",
        1,
        br#"{"size":1,"colour":"red","extra":1}"#,
    );
    raw_put(&store, STATUS, "junk", 1, br#"{"phase":"pending"}"#);
    let found: Vec<_> = store
        .check()
        .unwrap()
        .into_iter()
        .map(|i| (i.table, i.key))
        .collect();
    let want: Vec<(String, String)> = [
        ("widget", "future"),
        ("widget", "lonely"),
        ("widget_status", "orphan"),
        ("widget", "junk"),
        ("widget", "dup"),
    ]
    .iter()
    .map(|(t, k)| ((*t).into(), (*k).into()))
    .collect();
    let mut found_sorted = found.clone();
    found_sorted.sort();
    let mut want_sorted = want;
    want_sorted.sort();
    assert_eq!(found_sorted, want_sorted, "{found:?}");
    // A read of a bad row is a fatal, typed error naming the row.
    let err = store.read(|r| r.widget("junk")).unwrap_err();
    assert!(err.is_fatal());
    assert!(
        matches!(&err, Error::Corrupt { key, .. } if key == "junk"),
        "{err}"
    );
}

#[test]
fn on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dassie.redb");
    assert!(matches!(Store::open(&path), Err(Error::NoStore(_))));
    fs::write(&path, b"").unwrap();
    assert!(matches!(Store::open(&path), Err(Error::NoStore(_))));

    let store = Store::init(&path).unwrap();
    let rev = create(&store, "a", None);
    assert!(matches!(Store::open(&path), Err(Error::AlreadyRunning)));
    drop(store);

    assert!(matches!(Store::init(&path), Err(Error::StoreExists(_))));
    let store = Store::open(&path).unwrap();
    let w = store.read(|r| r.widget("a")).unwrap().unwrap();
    assert_eq!(w.spec.revision, rev, "the epoch survives a reopen");

    // A store `check` refuses is not opened.
    raw_put(&store, SPEC, "lonely", 1, br#"{"size":1,"colour":"red"}"#);
    drop(store);
    assert!(matches!(Store::open(&path), Err(Error::Inconsistent(v)) if v.len() == 1));
}

/// The names of the tables in a store's file.
fn file_tables(store: &Store) -> Vec<String> {
    let tx = store.db.begin_read().unwrap();
    let mut names: Vec<_> = tx
        .list_tables()
        .unwrap()
        .map(|t| redb::TableHandle::name(&t).to_owned())
        .collect();
    names.sort();
    names
}

fn schema_tables() -> Vec<String> {
    let mut names: Vec<_> = tables().into_iter().map(|t| t.name).collect();
    names.sort();
    names
}

#[test]
fn an_interrupted_init_is_finished_by_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dassie.redb");
    // A redb file with no migration applied, as a crash during the first
    // one leaves it.
    drop(Database::builder().create(&path).unwrap());
    assert!(matches!(Store::open(&path), Err(Error::NoStore(_))));
    let store = Store::init(&path).unwrap();
    assert_eq!(file_tables(&store), schema_tables());
    assert_consistent(&store);
}

#[test]
fn an_older_store_is_copied_then_migrated_to_the_same_tables_as_a_new_one() {
    const { assert!(SCHEMA_VERSION >= 2, "tests run with the widget migration") };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dassie.redb");
    Store::init_at(&path, 1);
    // An init interrupted after its first migration left an older store,
    // which init refuses and open upgrades.
    assert!(matches!(Store::init(&path), Err(Error::StoreExists(_))));
    let store = Store::open(&path).unwrap();
    assert!(dir.path().join("dassie.redb.v1").exists());
    assert!(!dir.path().join("dassie.redb.v1.tmp").exists());
    assert_eq!(stored_version(&store.db).unwrap(), SCHEMA_VERSION);
    assert_eq!(file_tables(&store), schema_tables());
    assert_eq!(file_tables(&Store::in_memory().unwrap()), schema_tables());
    create(&store, "a", None);
    drop(store);
    // The copy is the store as it was: version 1, no widget tables.
    let old = Database::builder()
        .open(dir.path().join("dassie.redb.v1"))
        .unwrap();
    assert_eq!(stored_version(&old).unwrap(), 1);
}

#[test]
fn check_holds_the_file_to_the_schemas_tables() {
    let store = Store::in_memory().unwrap();
    let tx = store.db.begin_write().unwrap();
    tx.open_table(Rows::new("stray")).unwrap();
    tx.commit().unwrap();
    let found = store.check().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].table, "stray");

    let tx = store.db.begin_write().unwrap();
    tx.delete_table(Rows::new("stray")).unwrap();
    tx.delete_table(STATUS).unwrap();
    tx.commit().unwrap();
    let found = store.check().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].table, "widget_status");
    assert!(found[0].problem.contains("missing"), "{found:?}");
}

#[test]
fn check_refuses_a_multimap_table() {
    let store = Store::in_memory().unwrap();
    let tx = store.db.begin_write().unwrap();
    tx.open_multimap_table(redb::MultimapTableDefinition::<&str, &str>::new("stray"))
        .unwrap();
    tx.commit().unwrap();
    let found = store.check().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].table, "stray");
    assert!(found[0].problem.contains("multimap"), "{found:?}");
}

#[test]
fn a_store_newer_than_the_binary_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dassie.redb");
    let store = Store::init(&path).unwrap();
    let tx = store.db.begin_write().unwrap();
    tx.open_table(META)
        .unwrap()
        .insert(META_VERSION, SCHEMA_VERSION + 1)
        .unwrap();
    tx.commit().unwrap();
    drop(store);
    assert!(matches!(
        Store::open(&path),
        Err(Error::TooNew { found, .. }) if found == SCHEMA_VERSION + 1
    ));
}

/// An in-memory backend that fails every write and sync once armed. Clones
/// share the bytes, so a fresh database can reopen what a failed one left.
#[derive(Debug, Clone)]
struct Failing {
    inner: Arc<InMemoryBackend>,
    /// Writes and syncs allowed before the next one fails; `u64::MAX` is
    /// never.
    left: Arc<AtomicU64>,
}

impl Failing {
    fn tick(&self) -> std::io::Result<()> {
        let left = self.left.load(Ordering::SeqCst);
        if left == 0 {
            return Err(std::io::Error::other("injected fault"));
        }
        if left != u64::MAX {
            self.left.store(left - 1, Ordering::SeqCst);
        }
        Ok(())
    }
}

impl StorageBackend for Failing {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.tick()?;
        self.inner.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.tick()?;
        self.inner.write(offset, data)
    }
}

/// Fails the Nth write or sync of a commit, for every N until the commit
/// gets through.
///
/// What redb guarantees, and what the daemon must assume: the write is
/// atomic, but an error from it does not mean nothing was written. A fault
/// late in the commit can come after the new state is already durable.
/// Either way later writes fail until the store is reopened, while reads
/// still serve the last state redb trusts.
#[test]
fn a_storage_fault_is_fatal_and_leaves_one_state_or_the_other() {
    let (mut lost, mut kept) = (0, 0);
    for allowed in 0..100 {
        let backend = Failing {
            inner: Arc::new(InMemoryBackend::new()),
            left: Arc::new(AtomicU64::new(u64::MAX)),
        };
        let store = Store::with_backend(backend.clone()).unwrap();
        create(&store, "a", None);
        backend.left.store(allowed, Ordering::SeqCst);
        let out = store.write(|w| w.create_widget("b", &spec(1, None), &pending()));
        let Err(err) = out else {
            assert!(lost > 0, "no fault lost the commit");
            return;
        };
        assert!(err.is_fatal(), "{err}");
        assert!(matches!(err, Error::Storage(_)), "{err}");
        let again = store.write(|w| w.create_widget("c", &spec(1, None), &pending()));
        assert!(
            matches!(again, Err(Error::Storage(_))),
            "a write after a fault must fail: {again:?}"
        );
        assert!(store.read(|r| r.widget("a")).unwrap().is_some());
        drop(store);

        backend.left.store(u64::MAX, Ordering::SeqCst);
        let store = Store::reopen_backend(backend).unwrap();
        assert_consistent(&store);
        assert!(store.read(|r| r.widget("a")).unwrap().is_some());
        // Exactly the state before the write, or exactly the state after.
        let revision = store.read(Reads::revision).unwrap().n;
        match store.read(|r| r.widget("b")).unwrap() {
            None => {
                lost += 1;
                assert_eq!(revision, 1, "fault after {allowed}");
            }
            Some(b) => {
                kept += 1;
                assert_eq!(revision, 2, "fault after {allowed}");
                assert_eq!(b.spec.revision.n, 2, "fault after {allowed}");
                assert_eq!(b.status.revision.n, 2, "fault after {allowed}");
            }
        }
        assert!(kept <= 1 || lost > 0, "fault after {allowed}");
    }
    panic!("the commit never got through");
}

/// Every stored type's JSON Schema, as of `SCHEMA_VERSION`. A change in
/// shape without a version bump fails here. To accept a change after
/// bumping the version, run with `DASSIE_UPDATE_SCHEMA=1`.
#[test]
fn schema_snapshot() {
    let schemas = serde_json::json!({
        "tables": tables(),
        "values": {
            "widget": schemars::schema_for!(WidgetSpec),
            "widget_status": schemars::schema_for!(WidgetStatus),
        },
    });
    let got = serde_json::to_string_pretty(&schemas).unwrap() + "\n";
    let path = format!(
        "{}/src/store/schema/v{SCHEMA_VERSION}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var_os("DASSIE_UPDATE_SCHEMA").is_some() {
        fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
        fs::write(&path, &got).unwrap();
    }
    let want = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{path}: {e}; bump SCHEMA_VERSION and set DASSIE_UPDATE_SCHEMA=1")
    });
    assert!(
        got == want,
        "the stored types changed shape: bump SCHEMA_VERSION, add a migration, \
         and regenerate {path} with DASSIE_UPDATE_SCHEMA=1"
    );
}
