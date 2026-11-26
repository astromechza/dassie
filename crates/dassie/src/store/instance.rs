//! Instances (ADR 0014): one microVM, keyed by a ULID. Its spec never
//! changes; it holds one host-wide slot from creation until its row is
//! deleted after teardown.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher, RandomState};

use serde::{Deserialize, Serialize};

use super::app::{self, AppReads};
use super::check::{decode_all, problem};
use super::{
    Error, Inconsistency, Precondition, ReadTxn, Reads, Result, Revision, Rows, Versioned,
    WriteTxn, get, scan, table_name,
};

pub(super) const SPEC: Rows = Rows::new("instance");
pub(super) const STATUS: Rows = Rows::new("instance_status");

/// Slots are 0 to this: ADR 0008's index and ADR 0013's uid offset.
pub(crate) const MAX_SLOT: u16 = 32767;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct InstanceSpec {
    pub(crate) app: String,
    /// The hash of the release this instance runs, for its whole life.
    pub(crate) release: String,
    /// The TAP, /31 and MAC (ADR 0008) and the jail uid, 0x70000000 + slot
    /// (ADR 0013).
    pub(crate) slot: u16,
    /// The guest assets present at creation. They record drift; they do not
    /// pin the files.
    pub(crate) kernel_digest: String,
    pub(crate) initrd_digest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct InstanceStatus {
    /// Unix milliseconds; set when the instance is to go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deleting_since: Option<u64>,
}

/// An instance's spec and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instance {
    pub(crate) spec: Versioned<InstanceSpec>,
    pub(crate) status: Versioned<InstanceStatus>,
}

/// Crockford's base32, as ULIDs use it.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A new ULID for `now` (Unix milliseconds), in its 26-character form. It
/// sorts by creation time. The 80 random bits come from std's `RandomState`,
/// which is seeded from the OS: unique, not secret.
pub(crate) fn new_id(now: u64) -> String {
    let mut random = 0u128;
    for _ in 0..2 {
        random = (random << 64) | u128::from(RandomState::new().build_hasher().finish());
    }
    encode_ulid(now, random)
}

fn encode_ulid(now: u64, random: u128) -> String {
    let value = (u128::from(now & 0xFFFF_FFFF_FFFF) << 80) | (random & ((1 << 80) - 1));
    (0..26)
        .rev()
        .map(|i| CROCKFORD[usize::try_from((value >> (i * 5)) & 31).expect("5 bits")] as char)
        .collect()
}

/// A ULID in canonical form: 26 Crockford base32 characters, the first at
/// most 7 so the value fits 128 bits.
pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 26 && id.bytes().all(|b| CROCKFORD.contains(&b)) && id.as_bytes()[0] <= b'7'
}

/// Reads of instances, for any transaction.
pub(crate) trait InstanceReads: Reads + Sized {
    fn instance(&self, id: &str) -> Result<Option<Instance>> {
        self.guarded(|| {
            let Some(spec) = get(self, SPEC, id)? else {
                return Ok(None);
            };
            let status = get(self, STATUS, id)?.ok_or_else(|| missing_status(id))?;
            Ok(Some(Instance { spec, status }))
        })
    }

    /// Every instance, oldest first, since ULIDs sort by creation time.
    fn instances(&self) -> Result<Vec<(String, Instance)>> {
        self.guarded(|| {
            let mut statuses: BTreeMap<String, Versioned<InstanceStatus>> =
                scan(self, STATUS)?.into_iter().collect();
            scan::<InstanceSpec>(self, SPEC)?
                .into_iter()
                .map(|(id, spec)| {
                    let status = statuses.remove(&id).ok_or_else(|| missing_status(&id))?;
                    Ok((id, Instance { spec, status }))
                })
                .collect()
        })
    }

    /// One app's instances, oldest first.
    fn instances_of(&self, app: &str) -> Result<Vec<(String, Instance)>> {
        Ok(self
            .instances()?
            .into_iter()
            .filter(|(_, i)| i.spec.value.app == app)
            .collect())
    }
}

impl<R: Reads> InstanceReads for R {}

fn missing_status(id: &str) -> Error {
    Error::Corrupt {
        table: table_name(STATUS),
        key: id.into(),
        reason: "spec without status".into(),
    }
}

fn not_found(id: &str) -> Error {
    Error::NotFound {
        table: table_name(SPEC),
        key: id.into(),
    }
}

impl WriteTxn {
    /// Creates an instance of the app's current release, on the lowest slot
    /// that no instance row holds and that `in_use_on_host` does not list.
    /// `app_pre` is the app's spec as the pass read it. Returns the slot.
    pub(crate) fn create_instance(
        &mut self,
        id: &str,
        app: &str,
        kernel_digest: &str,
        initrd_digest: &str,
        in_use_on_host: &BTreeSet<u16>,
        app_pre: Precondition,
    ) -> Result<u16> {
        self.op(|w| {
            if !valid_id(id) {
                return Err(Error::Invalid {
                    what: "instance id".into(),
                    reason: format!("{id:?} is not a canonical ULID"),
                });
            }
            let a = w.app(app)?.ok_or_else(|| app::not_found(app))?;
            w.require(app::SPEC, app, app_pre)?;
            if a.status.value.deleting_since.is_some() {
                return Err(Error::Deleting {
                    table: table_name(app::SPEC),
                    key: app.into(),
                });
            }
            if super::row_revision(w, SPEC, id)?.is_some() {
                return Err(Error::AlreadyExists {
                    table: table_name(SPEC),
                    key: id.into(),
                });
            }
            let held: BTreeSet<u16> = w
                .instances()?
                .iter()
                .map(|(_, i)| i.spec.value.slot)
                .collect();
            let slot = (0..=MAX_SLOT)
                .find(|s| !held.contains(s) && !in_use_on_host.contains(s))
                .ok_or(Error::Exhausted { what: "slot" })?;
            let spec = InstanceSpec {
                app: app.into(),
                release: a.spec.value.release,
                slot,
                kernel_digest: kernel_digest.into(),
                initrd_digest: initrd_digest.into(),
            };
            w.put(SPEC, id, &spec)?;
            w.put(STATUS, id, &InstanceStatus::default())?;
            Ok(slot)
        })
    }

    /// Marks an instance for deletion, if it and its app's spec are still at
    /// the releases the pass read (`app_pre` is `Any` when the app itself is
    /// being deleted).
    pub(crate) fn mark_instance_deleting(
        &mut self,
        id: &str,
        pre: Precondition,
        app_pre: Precondition,
        now: u64,
    ) -> Result<Revision> {
        self.op(|w| {
            let i = w.instance(id)?.ok_or_else(|| not_found(id))?;
            w.require(SPEC, id, pre)?;
            w.require(app::SPEC, &i.spec.value.app, app_pre)?;
            w.put(
                STATUS,
                id,
                &InstanceStatus {
                    deleting_since: Some(now),
                },
            )
        })
    }

    /// Withdraws a deletion mark before the instance's unit was stopped.
    pub(crate) fn unmark_instance(&mut self, id: &str, pre: Precondition) -> Result<Revision> {
        self.op(|w| {
            w.instance(id)?.ok_or_else(|| not_found(id))?;
            w.require(STATUS, id, pre)?;
            w.put(STATUS, id, &InstanceStatus::default())
        })
    }

    /// Deletes an instance's rows once its teardown is confirmed, which frees
    /// its slot, and prunes its release if nothing else uses it.
    pub(crate) fn delete_instance(&mut self, id: &str, pre: Precondition) -> Result<()> {
        self.op(|w| {
            let i = w.instance(id)?.ok_or_else(|| not_found(id))?;
            w.require(STATUS, id, pre)?;
            if i.status.value.deleting_since.is_none() {
                return Err(Error::Invalid {
                    what: "instance deletion".into(),
                    reason: format!("{id} is not marked for deletion"),
                });
            }
            w.remove(SPEC, id)?;
            w.remove(STATUS, id)?;
            w.prune_release(&i.spec.value.app, &i.spec.value.release)
        })
    }
}

pub(super) fn check(r: &ReadTxn, found: &mut Vec<Inconsistency>) -> Result<()> {
    let mut slots = BTreeMap::new();
    for (id, spec) in decode_all::<InstanceSpec>(r, SPEC, found)? {
        if !valid_id(&id) {
            found.push(problem(SPEC, &id, "the key is not a canonical ULID"));
        }
        let slot = spec.value.slot;
        if slot > MAX_SLOT {
            found.push(problem(
                SPEC,
                &id,
                &format!("slot {slot} is above {MAX_SLOT}"),
            ));
        }
        if let Some(first) = slots.insert(slot, id.clone()) {
            found.push(problem(
                SPEC,
                &id,
                &format!("slot {slot} is also held by {first}"),
            ));
        }
    }
    decode_all::<InstanceStatus>(r, STATUS, found)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulids_are_canonical_and_sort_by_time() {
        let a = new_id(1_700_000_000_000);
        let b = new_id(1_700_000_000_001);
        assert!(valid_id(&a) && valid_id(&b), "{a} {b}");
        assert!(a < b);
        assert_ne!(new_id(5), new_id(5));
        assert_eq!(encode_ulid(0, 0), "00000000000000000000000000");
        assert_eq!(
            encode_ulid(u64::MAX, u128::MAX),
            "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );
        assert!(!valid_id("8ZZZZZZZZZZZZZZZZZZZZZZZZZ"));
        assert!(!valid_id("0000000000000000000000000I"));
    }
}
