//! `Store::check`: everything an SQL engine would have checked (ADR 0016).

use std::collections::BTreeSet;

use serde::de::DeserializeOwned;

use super::{
    Inconsistency, KINDS, ReadTxn, Reads, Result, Revision, Rows, Versioned, decode, raw_scan,
    table_name, tables,
};

/// One resource kind, as `init` and `check` see it: its spec and status
/// tables, and the checks only the kind knows, such as decoding its rows and
/// its unique fields.
#[derive(Clone, Copy)]
pub(crate) struct KindDef {
    pub(crate) spec: Rows,
    pub(crate) status: Rows,
    pub(crate) check: fn(&ReadTxn, &mut Vec<Inconsistency>) -> Result<()>,
}

pub(super) fn run(r: &ReadTxn) -> Result<Vec<Inconsistency>> {
    let mut found = Vec::new();
    // The file must hold exactly the current schema's tables: a missing one
    // is a forgotten migration, an extra one a stale or foreign store.
    let want: BTreeSet<String> = tables().into_iter().map(|t| t.name).collect();
    let have: BTreeSet<String> =
        r.tx.list_tables()?
            .map(|t| redb::TableHandle::name(&t).to_owned())
            .collect();
    for name in want.difference(&have) {
        found.push(table_problem(name, "is missing: no migration created it"));
    }
    for name in have.difference(&want) {
        found.push(table_problem(name, "is not part of this schema"));
    }
    // redb lists multimap tables apart, and this schema has none.
    for t in r.tx.list_multimap_tables()? {
        found.push(table_problem(
            redb::MultimapTableHandle::name(&t),
            "is a multimap table, which this schema never has",
        ));
    }
    if want.difference(&have).next().is_some() {
        // Reads of a missing table would fail; report what we have.
        return Ok(found);
    }
    let store = r.revision()?;
    for kind in KINDS {
        let specs = keys_checking_revisions(r, kind.spec, store, &mut found)?;
        let statuses = keys_checking_revisions(r, kind.status, store, &mut found)?;
        for key in specs.difference(&statuses) {
            found.push(problem(kind.spec, key, "has no status row"));
        }
        for key in statuses.difference(&specs) {
            found.push(problem(kind.status, key, "has no spec row"));
        }
        (kind.check)(r, &mut found)?;
    }
    Ok(found)
}

fn keys_checking_revisions(
    r: &ReadTxn,
    table: Rows,
    store: Revision,
    found: &mut Vec<Inconsistency>,
) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for (key, (n, _)) in raw_scan(r, table)? {
        if n > store.n {
            found.push(problem(
                table,
                &key,
                &format!("revision {n} is above the store's {}", store.n),
            ));
        }
        keys.insert(key);
    }
    Ok(keys)
}

/// Every row of `table` that decodes. Each that does not is reported.
pub(crate) fn decode_all<T: DeserializeOwned>(
    r: &ReadTxn,
    table: Rows,
    found: &mut Vec<Inconsistency>,
) -> Result<Vec<(String, Versioned<T>)>> {
    let mut rows = Vec::new();
    for (key, row) in raw_scan(r, table)? {
        match decode(r, table, &key, row) {
            Ok(v) => rows.push((key, v)),
            Err(e) => found.push(problem(table, &key, &e.to_string())),
        }
    }
    Ok(rows)
}

pub(crate) fn problem(table: Rows, key: &str, problem: &str) -> Inconsistency {
    Inconsistency {
        table: table_name(table),
        key: key.into(),
        problem: problem.into(),
    }
}

fn table_problem(name: &str, problem: &str) -> Inconsistency {
    Inconsistency {
        table: name.into(),
        key: String::new(),
        problem: format!("table {problem}"),
    }
}
