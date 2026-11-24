//! A resource kind that exists only in tests. It exercises the store's
//! plumbing the way a real kind will, without settling the data model ahead
//! of its ADR: a spec and a status, a unique field, and preconditions on
//! every write.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::check::{decode_all, problem};
use super::{
    Error, Inconsistency, KindDef, Precondition, ReadTxn, Reads, Result, Revision, Rows, Versioned,
    WriteTxn, get, scan, table_name,
};

pub(super) const SPEC: Rows = Rows::new("widget");
pub(super) const STATUS: Rows = Rows::new("widget_status");

/// The test-only migration that creates widget tables, after the real
/// migrations, the way a real kind's own migration would.
pub(super) const MIGRATION: super::Migration = |tx| {
    tx.open_table(SPEC)?;
    tx.open_table(STATUS)?;
    Ok(())
};

pub(super) const KIND: KindDef = KindDef {
    spec: SPEC,
    status: STATUS,
    check,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct WidgetSpec {
    pub(crate) size: u32,
    pub(crate) colour: Colour,
    /// Unique across widgets when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub(crate) enum Colour {
    Red,
    Blue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct WidgetStatus {
    pub(crate) phase: Phase,
    /// The spec revision number this status was written for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Pending,
    Ready,
}

/// A widget's spec and status, each with its own revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Widget {
    pub(crate) spec: Versioned<WidgetSpec>,
    pub(crate) status: Versioned<WidgetStatus>,
}

/// Reads of widgets, for any transaction.
pub(crate) trait WidgetReads: Reads + Sized {
    fn widget(&self, key: &str) -> Result<Option<Widget>> {
        let Some(spec) = get(self, SPEC, key)? else {
            return Ok(None);
        };
        let status = get(self, STATUS, key)?.ok_or_else(|| Error::Corrupt {
            table: table_name(STATUS),
            key: key.into(),
            reason: "spec without status".into(),
        })?;
        Ok(Some(Widget { spec, status }))
    }

    /// Widgets in key order.
    fn widgets(&self) -> Result<Vec<(String, Widget)>> {
        let mut statuses: BTreeMap<_, _> = scan(self, STATUS)?.into_iter().collect();
        scan(self, SPEC)?
            .into_iter()
            .map(|(key, spec)| {
                let status = statuses.remove(&key).ok_or_else(|| Error::Corrupt {
                    table: table_name(STATUS),
                    key: key.clone(),
                    reason: "spec without status".into(),
                })?;
                Ok((key, Widget { spec, status }))
            })
            .collect()
    }
}

impl<R: Reads> WidgetReads for R {}

impl WriteTxn {
    pub(crate) fn create_widget(
        &mut self,
        key: &str,
        spec: &WidgetSpec,
        status: &WidgetStatus,
    ) -> Result<Revision> {
        self.op(|w| {
            if get::<WidgetSpec>(w, SPEC, key)?.is_some() {
                return Err(Error::AlreadyExists {
                    table: table_name(SPEC),
                    key: key.into(),
                });
            }
            unique_label(w, key, spec)?;
            w.put(SPEC, key, spec)?;
            w.put(STATUS, key, status)
        })
    }

    pub(crate) fn replace_widget_spec(
        &mut self,
        key: &str,
        spec: &WidgetSpec,
        pre: Precondition,
    ) -> Result<Revision> {
        self.op(|w| {
            exists(w, key)?;
            w.require(SPEC, key, pre)?;
            unique_label(w, key, spec)?;
            w.put(SPEC, key, spec)
        })
    }

    /// Sets the status if the status row matches `pre` and the spec row
    /// matches `spec_pre`, so a result is never recorded against a spec
    /// that has changed since.
    pub(crate) fn set_widget_status(
        &mut self,
        key: &str,
        status: &WidgetStatus,
        pre: Precondition,
        spec_pre: Precondition,
    ) -> Result<Revision> {
        self.op(|w| {
            exists(w, key)?;
            w.require(SPEC, key, spec_pre)?;
            w.require(STATUS, key, pre)?;
            w.put(STATUS, key, status)
        })
    }

    pub(crate) fn delete_widget(&mut self, key: &str, pre: Precondition) -> Result<()> {
        self.op(|w| {
            exists(w, key)?;
            w.require(SPEC, key, pre)?;
            w.remove(SPEC, key)?;
            w.remove(STATUS, key)?;
            Ok(())
        })
    }
}

fn exists(w: &WriteTxn, key: &str) -> Result<()> {
    if super::row_revision(w, SPEC, key)?.is_none() {
        return Err(Error::NotFound {
            table: table_name(SPEC),
            key: key.into(),
        });
    }
    Ok(())
}

/// Labels are unique: a scan, since there are no stored indexes.
fn unique_label(w: &WriteTxn, key: &str, spec: &WidgetSpec) -> Result<()> {
    let Some(label) = &spec.label else {
        return Ok(());
    };
    for (other, row) in scan::<WidgetSpec>(w, SPEC)? {
        if other != key && row.value.label.as_ref() == Some(label) {
            return Err(Error::Conflict {
                table: table_name(SPEC),
                field: "label",
                value: label.clone(),
                holder: other,
            });
        }
    }
    Ok(())
}

fn check(r: &ReadTxn, found: &mut Vec<Inconsistency>) -> Result<()> {
    let specs = decode_all::<WidgetSpec>(r, SPEC, found)?;
    decode_all::<WidgetStatus>(r, STATUS, found)?;
    let mut labels = BTreeMap::new();
    for (key, spec) in specs {
        if let Some(label) = spec.value.label
            && let Some(first) = labels.insert(label.clone(), key.clone())
        {
            found.push(problem(
                SPEC,
                &key,
                &format!("label {label:?} is also held by {first:?}"),
            ));
        }
    }
    Ok(())
}
