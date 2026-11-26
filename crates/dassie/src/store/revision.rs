//! Revisions (ADR 0014): one immutable template of an app, keyed by
//! `<app>/<hash>`, where the hash is the sha256 of the template's canonical
//! JSON.

use std::collections::BTreeMap;

use dassie_image::{Digest, Reference};
use serde::{Deserialize, Serialize};

use super::check::{decode_all, problem};
use super::{
    Error, Inconsistency, ReadTxn, Reads, Result, Rows, Versioned, WriteTxn, get, scan, table_name,
};

pub(super) const SPEC: Rows = Rows::new("revision");
pub(super) const STATUS: Rows = Rows::new("revision_status");

/// Everything rendered into an instance's files. A change to any field is a
/// new revision. Fields at their default are left out when stored, so a new
/// field with a default changes no existing hash. Defaults are frozen once
/// released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct Template {
    /// `registry/repository[:tag]@sha256:…`, normalised, with the platform
    /// manifest's digest.
    pub(crate) image: String,
    #[serde(default = "default_vcpus", skip_serializing_if = "is_default_vcpus")]
    pub(crate) vcpus: u32,
    #[serde(default = "default_memory", skip_serializing_if = "is_default_memory")]
    pub(crate) memory_mib: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cmd: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) readiness_port: Option<u16>,
    #[serde(
        default = "default_sigterm",
        skip_serializing_if = "is_default_sigterm"
    )]
    pub(crate) sigterm_timeout_seconds: u8,
    #[serde(
        default = "default_network",
        skip_serializing_if = "is_default_network"
    )]
    pub(crate) network: bool,
}

const DEFAULT_VCPUS: u32 = 1;
const DEFAULT_MEMORY_MIB: u32 = 256;
const DEFAULT_SIGTERM_SECONDS: u8 = 10;

fn default_vcpus() -> u32 {
    DEFAULT_VCPUS
}
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_default_vcpus(v: &u32) -> bool {
    *v == DEFAULT_VCPUS
}
fn default_memory() -> u32 {
    DEFAULT_MEMORY_MIB
}
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_default_memory(v: &u32) -> bool {
    *v == DEFAULT_MEMORY_MIB
}
fn default_sigterm() -> u8 {
    DEFAULT_SIGTERM_SECONDS
}
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_default_sigterm(v: &u8) -> bool {
    *v == DEFAULT_SIGTERM_SECONDS
}
fn default_network() -> bool {
    true
}
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_default_network(v: &bool) -> bool {
    *v
}

impl Template {
    /// A template for `image` with every other field at its default.
    pub(crate) fn new(image: impl Into<String>) -> Template {
        Template {
            image: image.into(),
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            env: BTreeMap::new(),
            entrypoint: None,
            cmd: None,
            readiness_port: None,
            sigterm_timeout_seconds: DEFAULT_SIGTERM_SECONDS,
            network: true,
        }
    }

    /// Checks every field against ADR 0014's bounds.
    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |reason: String| {
            Err(Error::Invalid {
                what: "template".into(),
                reason,
            })
        };
        match self.image.parse::<Reference>() {
            Ok(r) if r.digest().is_some() && r.to_string() == self.image => {}
            Ok(_) => {
                return invalid(format!(
                    "image {:?} must be normalised and pinned to a digest",
                    self.image
                ));
            }
            Err(e) => return invalid(e.to_string()),
        }
        if !(1..=32).contains(&self.vcpus) {
            return invalid(format!("vcpus {} is not 1 to 32", self.vcpus));
        }
        if !(128..=32768).contains(&self.memory_mib) {
            return invalid(format!(
                "memory_mib {} is not 128 to 32768",
                self.memory_mib
            ));
        }
        for (name, value) in &self.env {
            if !env_name_ok(name) {
                return invalid(format!(
                    "env name {name:?} must match [A-Za-z_][A-Za-z0-9_]* and not start with DASSIE_"
                ));
            }
            if value.contains('\0') {
                return invalid(format!("env {name} contains NUL"));
            }
        }
        for (field, argv) in [("entrypoint", &self.entrypoint), ("cmd", &self.cmd)] {
            if let Some(argv) = argv {
                if argv.is_empty() {
                    return invalid(format!("{field} is empty; leave it out instead"));
                }
                if argv.iter().any(|a| a.contains('\0')) {
                    return invalid(format!("{field} contains NUL"));
                }
            }
        }
        if self.readiness_port == Some(0) {
            return invalid("readiness_port 0 is not 1 to 65535".into());
        }
        if self.sigterm_timeout_seconds > 10 {
            return invalid(format!(
                "sigterm_timeout_seconds {} is not 0 to 10",
                self.sigterm_timeout_seconds
            ));
        }
        Ok(())
    }

    /// The canonical JSON: defaults left out, object keys sorted, no
    /// whitespace. Serialising through `serde_json::Value` sorts keys, since
    /// `serde_json`'s `preserve_order` feature is off.
    pub(crate) fn canonical(&self) -> Vec<u8> {
        let value = serde_json::to_value(self).expect("a template always serialises");
        serde_json::to_vec(&value).expect("a JSON value always serialises")
    }

    /// The revision's hash: the sha256 of the canonical JSON, in hex.
    pub(crate) fn hash(&self) -> String {
        Digest::of(&self.canonical()).hex().to_owned()
    }
}

fn env_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with("DASSIE_")
}

/// When the revision was first applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct RevisionStatus {
    /// Unix milliseconds.
    pub(crate) created_at: u64,
}

/// A revision's key: `<app>/<hash>`.
pub(crate) fn key(app: &str, hash: &str) -> String {
    format!("{app}/{hash}")
}

/// A revision's template and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevisionRow {
    pub(crate) template: Versioned<Template>,
    pub(crate) status: Versioned<RevisionStatus>,
}

/// Reads of revisions, for any transaction.
pub(crate) trait RevisionReads: Reads + Sized {
    fn app_revision(&self, app: &str, hash: &str) -> Result<Option<RevisionRow>> {
        self.guarded(|| {
            let key = key(app, hash);
            let Some(template) = get(self, SPEC, &key)? else {
                return Ok(None);
            };
            let status = get(self, STATUS, &key)?.ok_or_else(|| missing_status(&key))?;
            Ok(Some(RevisionRow { template, status }))
        })
    }

    /// One app's revisions, by hash.
    fn app_revisions(&self, app: &str) -> Result<Vec<(String, RevisionRow)>> {
        self.guarded(|| {
            let prefix = format!("{app}/");
            let mut statuses: BTreeMap<String, Versioned<RevisionStatus>> =
                scan(self, STATUS)?.into_iter().collect();
            let mut out = Vec::new();
            for (key, template) in scan::<Template>(self, SPEC)? {
                let Some(hash) = key.strip_prefix(&prefix) else {
                    continue;
                };
                let status = statuses.remove(&key).ok_or_else(|| missing_status(&key))?;
                out.push((hash.to_owned(), RevisionRow { template, status }));
            }
            Ok(out)
        })
    }
}

impl<R: Reads> RevisionReads for R {}

fn missing_status(key: &str) -> Error {
    Error::Corrupt {
        table: table_name(STATUS),
        key: key.into(),
        reason: "spec without status".into(),
    }
}

impl WriteTxn {
    /// Creates the revision unless one with its hash exists, and returns the
    /// hash. Within `op`.
    pub(super) fn ensure_revision(
        &mut self,
        app: &str,
        template: &Template,
        now: u64,
    ) -> Result<String> {
        template.validate()?;
        let hash = template.hash();
        let key = key(app, &hash);
        if super::row_revision(self, SPEC, &key)?.is_none() {
            self.put(SPEC, &key, template)?;
            self.put(STATUS, &key, &RevisionStatus { created_at: now })?;
        }
        Ok(hash)
    }

    /// Removes a revision's rows. Within `op`; the caller checks it is
    /// unused.
    pub(super) fn remove_revision(&mut self, app: &str, hash: &str) -> Result<()> {
        let key = key(app, hash);
        self.remove(SPEC, &key)?;
        self.remove(STATUS, &key)?;
        Ok(())
    }
}

pub(super) fn check(r: &ReadTxn, found: &mut Vec<Inconsistency>) -> Result<()> {
    for (key, template) in decode_all::<Template>(r, SPEC, found)? {
        let Some((_, hash)) = key.split_once('/') else {
            found.push(problem(SPEC, &key, "key is not <app>/<hash>"));
            continue;
        };
        if template.value.hash() != hash {
            found.push(problem(SPEC, &key, "the template does not hash to its key"));
        }
        if let Err(e) = template.value.validate() {
            found.push(problem(SPEC, &key, &e.to_string()));
        }
    }
    decode_all::<RevisionStatus>(r, STATUS, found)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const IMAGE: &str = "docker.io/library/nginx:1.27@sha256:4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c";

    #[test]
    fn defaults_are_left_out_of_the_canonical_form() {
        let mut t = Template::new(IMAGE);
        t.env.insert("GREETING".into(), "hello world".into());
        let json = String::from_utf8(t.canonical()).unwrap();
        assert_eq!(
            json,
            format!(r#"{{"env":{{"GREETING":"hello world"}},"image":"{IMAGE}"}}"#)
        );
        // Spelling a default out gives the same hash.
        let spelt: Template = serde_json::from_str(&format!(
            r#"{{"image":"{IMAGE}","vcpus":1,"network":true}}"#
        ))
        .unwrap();
        assert_eq!(spelt.hash(), Template::new(IMAGE).hash());
        assert_ne!(t.hash(), Template::new(IMAGE).hash());
        assert_eq!(t.hash().len(), 64);
    }

    #[test]
    fn validation() {
        assert!(Template::new(IMAGE).validate().is_ok());
        let bad = |f: &dyn Fn(&mut Template)| {
            let mut t = Template::new(IMAGE);
            f(&mut t);
            t.validate().unwrap_err().to_string()
        };
        assert!(bad(&|t| t.image = "nginx:1.27".into()).contains("pinned"));
        assert!(bad(&|t| t.image = IMAGE.replace("docker.io/library/", "")).contains("normalised"));
        assert!(bad(&|t| t.vcpus = 0).contains("vcpus"));
        assert!(bad(&|t| t.memory_mib = 64).contains("memory_mib"));
        assert!(
            bad(&|t| {
                t.env.insert("DASSIE_X".into(), "1".into());
            })
            .contains("DASSIE_")
        );
        assert!(
            bad(&|t| {
                t.env.insert("1X".into(), "1".into());
            })
            .contains("env name")
        );
        assert!(bad(&|t| t.cmd = Some(vec![])).contains("empty"));
        assert!(bad(&|t| t.entrypoint = Some(vec!["a\0".into()])).contains("NUL"));
        assert!(bad(&|t| t.readiness_port = Some(0)).contains("readiness_port"));
        assert!(bad(&|t| t.sigterm_timeout_seconds = 11).contains("sigterm"));
    }
}
