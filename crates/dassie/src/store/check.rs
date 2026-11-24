//! `Store::check`: everything an SQL engine would have checked (ADR 0016).

use std::collections::BTreeSet;

use serde::de::DeserializeOwned;

use super::{
    Inconsistency, KINDS, ReadTxn, Reads, Result, Revision, Rows, Versioned, decode, raw_scan,
    table_name,
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
