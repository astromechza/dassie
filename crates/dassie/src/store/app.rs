//! Apps (ADR 0014): what the operator asks for, keyed by the app's name. An
//! app names its current release and how many instances it should run.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::check::{decode_all, problem};
use super::instance::{self, InstanceReads};
use super::release::{self, ReleaseReads, Template};
use super::{
    Error, Inconsistency, Precondition, ReadTxn, Reads, Result, Revision, Rows, Versioned,
    WriteTxn, get, scan, table_name,
};

pub(super) const SPEC: Rows = Rows::new("app");
pub(super) const STATUS: Rows = Rows::new("app_status");

/// The most replicas an app may ask for.
pub(crate) const MAX_REPLICAS: u32 = 64;

/// The kind of workload. Jobs (#27) add another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub(crate) enum AppKind {
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct AppSpec {
    pub(crate) kind: AppKind,
    /// How many instances should be ready.
    pub(crate) replicas: u32,
    /// The hash of the release the app should run.
    pub(crate) release: String,
}

/// What reconcile has recorded about an app. Counts are as of the last pass
/// that wrote them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct AppStatus {
    /// Unix milliseconds; set when the operator deleted the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deleting_since: Option<u64>,
    /// Unix milliseconds; set while instances of an older release remain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rolling_since: Option<u64>,
    #[serde(default)]
    pub(crate) current_instances: u32,
    #[serde(default)]
    pub(crate) old_instances: u32,
    #[serde(default)]
    pub(crate) deleting_instances: u32,
}

/// An app's spec and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct App {
    pub(crate) spec: Versioned<AppSpec>,
    pub(crate) status: Versioned<AppStatus>,
}

/// What an apply produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The release's hash: the app's `release` now.
    pub(crate) hash: String,
    /// The app spec row's new store revision, for a later precondition.
    pub(crate) spec: Revision,
}

/// `[a-z0-9][a-z0-9-]{0,31}`.
pub(crate) fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Reads of apps, for any transaction.
pub(crate) trait AppReads: Reads + Sized {
    fn app(&self, name: &str) -> Result<Option<App>> {
        self.guarded(|| {
            let Some(spec) = get(self, SPEC, name)? else {
                return Ok(None);
            };
            let status = get(self, STATUS, name)?.ok_or_else(|| missing_status(name))?;
            Ok(Some(App { spec, status }))
        })
    }

    /// Apps in name order.
    fn apps(&self) -> Result<Vec<(String, App)>> {
        self.guarded(|| {
            let mut statuses: BTreeMap<String, Versioned<AppStatus>> =
                scan(self, STATUS)?.into_iter().collect();
            scan::<AppSpec>(self, SPEC)?
                .into_iter()
                .map(|(name, spec)| {
                    let status = statuses
                        .remove(&name)
                        .ok_or_else(|| missing_status(&name))?;
                    Ok((name, App { spec, status }))
                })
                .collect()
        })
    }
}

impl<R: Reads> AppReads for R {}

fn missing_status(name: &str) -> Error {
    Error::Corrupt {
        table: table_name(STATUS),
        key: name.into(),
        reason: "spec without status".into(),
    }
}

pub(super) fn not_found(name: &str) -> Error {
    Error::NotFound {
        table: table_name(SPEC),
        key: name.into(),
    }
}

impl WriteTxn {
    /// Applies an app: creates its release unless one with the template's
    /// hash exists, and writes the app's spec. `pre` is the app's spec as
    /// read before the image was pulled, or `Absent` for a new app. An app
    /// being deleted is refused.
    pub(crate) fn apply_app(
        &mut self,
        name: &str,
        replicas: u32,
        template: &Template,
        pre: Precondition,
        now: u64,
    ) -> Result<Applied> {
        self.op(|w| {
            if !valid_name(name) {
                return Err(Error::Invalid {
                    what: "app name".into(),
                    reason: format!("{name:?} must match [a-z0-9][a-z0-9-]{{0,31}}"),
                });
            }
            if replicas > MAX_REPLICAS {
                return Err(Error::Invalid {
                    what: "replicas".into(),
                    reason: format!("{replicas} is above {MAX_REPLICAS}"),
                });
            }
            w.require(SPEC, name, pre)?;
            let existing = w.app(name)?;
            if existing
                .as_ref()
                .is_some_and(|a| a.status.value.deleting_since.is_some())
            {
                return Err(Error::Deleting {
                    table: table_name(SPEC),
                    key: name.into(),
                });
            }
            let hash = w.ensure_release(name, template, now)?;
            let spec = AppSpec {
                kind: AppKind::Service,
                replicas,
                release: hash.clone(),
            };
            // The same spec again changes nothing, not even the store revision.
            if let Some(old) = &existing
                && old.spec.value == spec
            {
                return Ok(Applied {
                    hash,
                    spec: old.spec.revision,
                });
            }
            let rev = w.put(SPEC, name, &spec)?;
            match existing {
                None => {
                    w.put(STATUS, name, &AppStatus::default())?;
                }
                Some(old) if old.spec.value.release != hash => {
                    w.prune_release(name, &old.spec.value.release)?;
                }
                Some(_) => {}
            }
            Ok(Applied { hash, spec: rev })
        })
    }

    /// Records what reconcile found, if the status and the spec are still
    /// at the releases the pass read.
    pub(crate) fn set_app_status(
        &mut self,
        name: &str,
        status: &AppStatus,
        pre: Precondition,
        spec_pre: Precondition,
    ) -> Result<Revision> {
        self.op(|w| {
            if w.app(name)?.is_none() {
                return Err(not_found(name));
            }
            w.require(SPEC, name, spec_pre)?;
            w.require(STATUS, name, pre)?;
            w.put(STATUS, name, status)
        })
    }

    /// Starts deleting an app. It cannot be undone.
    pub(crate) fn delete_app(&mut self, name: &str, pre: Precondition, now: u64) -> Result<()> {
        self.op(|w| {
            let app = w.app(name)?.ok_or_else(|| not_found(name))?;
            w.require(SPEC, name, pre)?;
            if app.status.value.deleting_since.is_none() {
                let status = AppStatus {
                    deleting_since: Some(now),
                    ..app.status.value
                };
                w.put(STATUS, name, &status)?;
            }
            Ok(())
        })
    }

    /// Removes an app being deleted, and its releases, once it has no
    /// instance rows left.
    pub(crate) fn remove_app(&mut self, name: &str) -> Result<()> {
        self.op(|w| {
            let app = w.app(name)?.ok_or_else(|| not_found(name))?;
            if app.status.value.deleting_since.is_none() {
                return Err(Error::Invalid {
                    what: "app removal".into(),
                    reason: format!("{name:?} is not being deleted"),
                });
            }
            if !w.instances_of(name)?.is_empty() {
                return Err(Error::StillReferenced {
                    table: table_name(SPEC),
                    key: name.into(),
                });
            }
            for (hash, _) in w.releases_of(name)? {
                w.remove_release(name, &hash)?;
            }
            w.remove(SPEC, name)?;
            w.remove(STATUS, name)?;
            Ok(())
        })
    }

    /// Removes a release once neither its app nor any instance uses it.
    /// Within `op`.
    pub(super) fn prune_release(&mut self, app: &str, hash: &str) -> Result<()> {
        let current = get::<AppSpec>(self, SPEC, app)?.is_some_and(|a| a.value.release == hash);
        let used = self
            .instances_of(app)?
            .iter()
            .any(|(_, i)| i.spec.value.release == hash);
        if !current && !used {
            self.remove_release(app, hash)?;
        }
        Ok(())
    }
}

/// Checks references across the three kinds, and each app's rows.
pub(super) fn check(r: &ReadTxn, found: &mut Vec<Inconsistency>) -> Result<()> {
    let apps = decode_all::<AppSpec>(r, SPEC, found)?;
    decode_all::<AppStatus>(r, STATUS, found)?;
    let names: BTreeSet<String> = apps.iter().map(|(n, _)| n.clone()).collect();
    let releases: BTreeSet<String> = decode_all::<Template>(r, release::SPEC, &mut Vec::new())?
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    for (name, spec) in &apps {
        if !valid_name(name) {
            found.push(problem(SPEC, name, "the name is not a valid app name"));
        }
        if !releases.contains(&release::key(name, &spec.value.release)) {
            found.push(problem(SPEC, name, "its release does not exist"));
        }
    }
    for key in &releases {
        let app = key.split_once('/').map_or(key.as_str(), |(a, _)| a);
        if !names.contains(app) {
            found.push(problem(release::SPEC, key, "its app does not exist"));
        }
    }
    let instances = decode_all::<instance::InstanceSpec>(r, instance::SPEC, &mut Vec::new())?;
    for (id, spec) in instances {
        let i = spec.value;
        if !names.contains(&i.app) {
            found.push(problem(instance::SPEC, &id, "its app does not exist"));
        }
        if !releases.contains(&release::key(&i.app, &i.release)) {
            found.push(problem(instance::SPEC, &id, "its release does not exist"));
        }
    }
    Ok(())
}
