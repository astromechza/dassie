//! The store's errors and `check`'s report.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use super::{Precondition, Revision};

/// What a store operation can fail with.
///
/// The typed variants abort the write and are the caller's to report or
/// retry. The fatal ones ([`Error::is_fatal`]) mean the store cannot be
/// trusted until it is reopened, so the daemon exits (ADR 0016).
#[derive(Debug, Clone)]
pub(crate) enum Error {
    /// No row with this key.
    NotFound { table: String, key: String },
    /// A row with this key already exists.
    AlreadyExists { table: String, key: String },
    /// Another row already holds a value that must be unique.
    Conflict {
        table: String,
        field: &'static str,
        value: String,
        holder: String,
    },
    /// A value is outside its bounds, such as a template field.
    Invalid { what: String, reason: String },
    /// Other rows still refer to this one.
    StillReferenced { table: String, key: String },
    /// The row is being deleted, which refuses this change.
    Deleting { table: String, key: String },
    /// No value of a host-wide allocation is free.
    Exhausted { what: &'static str },
    /// A precondition did not match the row.
    Stale {
        table: String,
        key: String,
        expected: Precondition,
        actual: Option<Revision>,
    },
    /// redb failed. After an I/O error redb fails every later operation, so
    /// this is always fatal.
    Storage(Arc<redb::Error>),
    /// A file operation around the store failed, such as the copy before a
    /// migration.
    Io(Arc<std::io::Error>),
    /// A row that does not decode, or a value that will not encode.
    Corrupt {
        table: String,
        key: String,
        reason: String,
    },
    /// Another process has the store open.
    AlreadyRunning,
    /// `open` found no store at this path: only `init` creates one.
    NoStore(PathBuf),
    /// `init` found a store at this path: a file a migration has run on.
    StoreExists(PathBuf),
    /// The store was written by a newer binary.
    TooNew { found: u64, supported: u64 },
    /// `open` ran `check`, and it found these.
    Inconsistent(Vec<Inconsistency>),
}

impl Error {
    /// Whether the daemon must exit rather than carry on.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::Storage(_) | Error::Io(_) | Error::Corrupt { .. } | Error::Inconsistent(_)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound { table, key } => write!(f, "{table} {key:?} not found"),
            Error::AlreadyExists { table, key } => write!(f, "{table} {key:?} already exists"),
            Error::Invalid { what, reason } => write!(f, "invalid {what}: {reason}"),
            Error::Deleting { table, key } => write!(f, "{table} {key:?} is being deleted"),
            Error::StillReferenced { table, key } => {
                write!(f, "{table} {key:?} is still referenced")
            }
            Error::Exhausted { what } => write!(f, "no free {what}"),
            Error::Conflict {
                table,
                field,
                value,
                holder,
            } => write!(f, "{table} {field} {value:?} is already held by {holder:?}"),
            Error::Stale {
                table,
                key,
                expected,
                actual,
            } => write!(
                f,
                "{table} {key:?} is stale: expected {expected:?}, found {actual:?}"
            ),
            Error::Storage(e) => write!(f, "store: {e}"),
            Error::Io(e) => write!(f, "store file: {e}"),
            Error::Corrupt { table, key, reason } => {
                write!(f, "store row {table} {key:?} is corrupt: {reason}")
            }
            Error::AlreadyRunning => f.write_str("the store is open in another process"),
            Error::NoStore(path) => {
                write!(f, "no store at {}; create one with init", path.display())
            }
            Error::StoreExists(path) => write!(
                f,
                "{} is already a store; open upgrades it if it is older",
                path.display()
            ),
            Error::TooNew { found, supported } => write!(
                f,
                "the store is at schema version {found}, newer than this binary's {supported}"
            ),
            Error::Inconsistent(found) => {
                write!(f, "the store is inconsistent ({} problems)", found.len())?;
                for i in found {
                    write!(f, "\n  {i}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Storage(e) => Some(e.as_ref()),
            Error::Io(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl<E: Into<redb::Error>> From<E> for Error {
    fn from(e: E) -> Self {
        Error::Storage(Arc::new(e.into()))
    }
}

/// One problem `check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inconsistency {
    pub(crate) table: String,
    pub(crate) key: String,
    pub(crate) problem: String,
}

impl fmt::Display for Inconsistency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?}: {}", self.table, self.key, self.problem)
    }
}
