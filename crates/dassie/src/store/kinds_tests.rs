//! Tests of the apps, revisions and instances kinds (ADR 0014).

use std::collections::BTreeSet;

use super::app::{AppReads, AppStatus, MAX_REPLICAS};
use super::instance::{InstanceReads, MAX_SLOT, new_id};
use super::revision::{RevisionReads, Template};
use super::{Error, Precondition, Reads, Store, raw_put_for_tests};

const IMAGE: &str = "docker.io/library/nginx:1.27@sha256:4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c4f1c";
const NOW: u64 = 1_700_000_000_000;
const KERNEL: &str = "sha256:kernel";
const INITRD: &str = "sha256:initrd";

fn template(greeting: &str) -> Template {
    let mut t = Template::new(IMAGE);
    t.env.insert("GREETING".into(), greeting.into());
    t
}

fn apply(store: &Store, name: &str, replicas: u32, t: &Template) -> Result<String, Error> {
    let pre = store
        .read(|r| r.app(name))?
        .map_or(Precondition::Absent, |a| Precondition::At(a.spec.revision));
    store.write(|w| w.apply_app(name, replicas, t, pre, NOW).map(|a| a.hash))
}

fn create(store: &Store, app: &str, host: &BTreeSet<u16>) -> Result<(String, u16), Error> {
    let id = new_id(NOW);
    let pre = store
        .read(|r| r.app(app))?
        .map_or(Precondition::Any, |a| Precondition::At(a.spec.revision));
    let slot = store.write(|w| w.create_instance(&id, app, KERNEL, INITRD, host, pre))?;
    Ok((id, slot))
}

fn mark_and_delete(store: &Store, id: &str) {
    let i = store.read(|r| r.instance(id)).unwrap().unwrap();
    store
        .write(|w| {
            w.mark_instance_deleting(
                id,
                Precondition::At(i.spec.revision),
                Precondition::Any,
                NOW,
            )
        })
        .unwrap();
    let i = store.read(|r| r.instance(id)).unwrap().unwrap();
    store
        .write(|w| w.delete_instance(id, Precondition::At(i.status.revision)))
        .unwrap();
}

fn assert_consistent(store: &Store) {
    assert_eq!(store.check().unwrap(), Vec::new());
}

#[test]
fn applying_creates_the_revision_and_points_the_app_at_it() {
    let store = Store::in_memory().unwrap();
    let hash = apply(&store, "web", 2, &template("hi")).unwrap();
    let app = store.read(|r| r.app("web")).unwrap().unwrap();
    assert_eq!(app.spec.value.revision, hash);
    assert_eq!(app.spec.value.replicas, 2);
    assert_eq!(app.status.value, AppStatus::default());
    let rev = store
        .read(|r| r.app_revision("web", &hash))
        .unwrap()
        .unwrap();
    assert_eq!(rev.template.value, template("hi"));
    assert_eq!(rev.status.value.created_at, NOW);
    assert_consistent(&store);
}

#[test]
fn the_same_spec_again_changes_nothing() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 2, &template("hi")).unwrap();
    let before = store.read(Reads::revision).unwrap();
    apply(&store, "web", 2, &template("hi")).unwrap();
    assert_eq!(store.read(Reads::revision).unwrap(), before);
}

#[test]
fn replicas_alone_reuse_the_revision() {
    let store = Store::in_memory().unwrap();
    let hash = apply(&store, "web", 2, &template("hi")).unwrap();
    assert_eq!(apply(&store, "web", 5, &template("hi")).unwrap(), hash);
    assert_eq!(store.read(|r| r.app_revisions("web")).unwrap().len(), 1);
    let app = store.read(|r| r.app("web")).unwrap().unwrap();
    assert_eq!(app.spec.value.replicas, 5);
}

#[test]
fn a_new_template_prunes_the_old_revision_unless_an_instance_runs_it() {
    let store = Store::in_memory().unwrap();
    let first = apply(&store, "web", 1, &template("one")).unwrap();
    let second = apply(&store, "web", 1, &template("two")).unwrap();
    assert_ne!(first, second);
    let hashes = |s: &Store| -> Vec<String> {
        s.read(|r| r.app_revisions("web"))
            .unwrap()
            .into_iter()
            .map(|(h, _)| h)
            .collect()
    };
    assert_eq!(hashes(&store), vec![second.clone()], "unused: pruned");

    let (id, _) = create(&store, "web", &BTreeSet::new()).unwrap();
    apply(&store, "web", 1, &template("three")).unwrap();
    assert!(
        hashes(&store).contains(&second),
        "an instance still runs it"
    );
    mark_and_delete(&store, &id);
    assert!(
        !hashes(&store).contains(&second),
        "pruned with its last instance"
    );
    assert_consistent(&store);
}

#[test]
fn apply_refusals() {
    let store = Store::in_memory().unwrap();
    let invalid = |r: Result<String, Error>| matches!(r, Err(Error::Invalid { .. }));
    assert!(invalid(apply(&store, "Web", 1, &template("x"))));
    assert!(invalid(apply(&store, "-web", 1, &template("x"))));
    assert!(invalid(apply(
        &store,
        "web",
        MAX_REPLICAS + 1,
        &template("x")
    )));
    assert!(invalid(apply(&store, "web", 1, &Template::new("nginx"))));

    apply(&store, "web", 1, &template("x")).unwrap();
    // A pre-pull read gone stale: another apply in between.
    let stale = store.read(|r| r.app("web")).unwrap().unwrap().spec.revision;
    apply(&store, "web", 2, &template("x")).unwrap();
    let err = store
        .write(|w| w.apply_app("web", 3, &template("x"), Precondition::At(stale), NOW))
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");

    // Being deleted: refused.
    store
        .write(|w| w.delete_app("web", Precondition::Any, NOW))
        .unwrap();
    let err = apply(&store, "web", 1, &template("y")).unwrap_err();
    assert!(matches!(err, Error::Deleting { .. }), "{err}");
}

#[test]
fn an_app_recreated_during_a_pull_is_stale() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 1, &template("x")).unwrap();
    let before_pull = store.read(|r| r.app("web")).unwrap().unwrap().spec.revision;
    // Deleted and recreated while the image was pulled.
    store
        .write(|w| w.delete_app("web", Precondition::Any, NOW))
        .unwrap();
    store.write(|w| w.remove_app("web")).unwrap();
    apply(&store, "web", 1, &template("x")).unwrap();
    let err = store
        .write(|w| w.apply_app("web", 2, &template("x"), Precondition::At(before_pull), NOW))
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");
}

#[test]
fn slots_are_the_lowest_free_and_skip_the_host() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 3, &template("x")).unwrap();
    let none = BTreeSet::new();
    let (a, slot_a) = create(&store, "web", &none).unwrap();
    let (_, slot_b) = create(&store, "web", &none).unwrap();
    assert_eq!((slot_a, slot_b), (0, 1));
    // The host still carries slot 2, say after a restore.
    let (_, slot_c) = create(&store, "web", &BTreeSet::from([2])).unwrap();
    assert_eq!(slot_c, 3);
    // A deleted instance's slot is free again.
    mark_and_delete(&store, &a);
    assert_eq!(create(&store, "web", &none).unwrap().1, 0);
    // Every slot taken.
    let all: BTreeSet<u16> = (0..=MAX_SLOT).collect();
    let err = create(&store, "web", &all).unwrap_err();
    assert!(matches!(err, Error::Exhausted { .. }), "{err}");
    assert_consistent(&store);
}

#[test]
fn instance_writes_check_their_preconditions() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 1, &template("x")).unwrap();
    let app_rev = store.read(|r| r.app("web")).unwrap().unwrap().spec.revision;
    // The app changed after the pass read it: the create is stale.
    apply(&store, "web", 2, &template("x")).unwrap();
    let err = store
        .write(|w| {
            w.create_instance(
                &new_id(NOW),
                "web",
                KERNEL,
                INITRD,
                &BTreeSet::new(),
                Precondition::At(app_rev),
            )
        })
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");

    let (id, _) = create(&store, "web", &BTreeSet::new()).unwrap();
    let i = store.read(|r| r.instance(&id)).unwrap().unwrap();
    // A mark under a changed app spec is stale, as rule 2 decided on R.
    let app_rev = store.read(|r| r.app("web")).unwrap().unwrap().spec.revision;
    apply(&store, "web", 3, &template("x")).unwrap();
    let err = store
        .write(|w| {
            w.mark_instance_deleting(
                &id,
                Precondition::At(i.spec.revision),
                Precondition::At(app_rev),
                NOW,
            )
        })
        .unwrap_err();
    assert!(matches!(err, Error::Stale { .. }), "{err}");
    // Deleting an unmarked instance is refused.
    let err = store
        .write(|w| w.delete_instance(&id, Precondition::Any))
        .unwrap_err();
    assert!(matches!(err, Error::Invalid { .. }), "{err}");
    // A withdrawn mark leaves the instance counted again.
    store
        .write(|w| w.mark_instance_deleting(&id, Precondition::Any, Precondition::Any, NOW))
        .unwrap();
    let marked = store
        .read(|r| r.instance(&id))
        .unwrap()
        .unwrap()
        .status
        .revision;
    store
        .write(|w| w.unmark_instance(&id, Precondition::At(marked)))
        .unwrap();
    let i = store.read(|r| r.instance(&id)).unwrap().unwrap();
    assert_eq!(i.status.value.deleting_since, None);
}

#[test]
fn an_app_being_deleted_creates_nothing_and_goes_once_empty() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 1, &template("x")).unwrap();
    let (id, _) = create(&store, "web", &BTreeSet::new()).unwrap();
    store
        .write(|w| w.delete_app("web", Precondition::Any, NOW))
        .unwrap();
    let err = create(&store, "web", &BTreeSet::new()).unwrap_err();
    assert!(matches!(err, Error::Deleting { .. }), "{err}");
    let err = store.write(|w| w.remove_app("web")).unwrap_err();
    assert!(matches!(err, Error::StillReferenced { .. }), "{err}");
    mark_and_delete(&store, &id);
    store.write(|w| w.remove_app("web")).unwrap();
    assert_eq!(store.read(|r| r.app("web")).unwrap(), None);
    assert_eq!(store.read(|r| r.app_revisions("web")).unwrap(), Vec::new());
    assert_consistent(&store);
}

#[test]
fn reconcile_records_its_counts() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 1, &template("x")).unwrap();
    let app = store.read(|r| r.app("web")).unwrap().unwrap();
    let status = AppStatus {
        rolling_since: Some(NOW),
        current_instances: 1,
        old_instances: 2,
        deleting_instances: 1,
        ..AppStatus::default()
    };
    store
        .write(|w| {
            w.set_app_status(
                "web",
                &status,
                Precondition::At(app.status.revision),
                Precondition::At(app.spec.revision),
            )
        })
        .unwrap();
    assert_eq!(
        store.read(|r| r.app("web")).unwrap().unwrap().status.value,
        status
    );
    assert_eq!(store.read(AppReads::apps).unwrap().len(), 1);
}

#[test]
fn check_finds_broken_references() {
    let store = Store::in_memory().unwrap();
    apply(&store, "web", 1, &template("x")).unwrap();
    let (id, _) = create(&store, "web", &BTreeSet::new()).unwrap();
    // An instance of an app that does not exist, on a taken slot.
    raw_put_for_tests(
        &store,
        super::instance::SPEC,
        "01HZZZZZZZZZZZZZZZZZZZZZZZ",
        1,
        br#"{"app":"ghost","revision":"abc","slot":0,"kernel_digest":"k","initrd_digest":"i"}"#,
    );
    raw_put_for_tests(
        &store,
        super::instance::STATUS,
        "01HZZZZZZZZZZZZZZZZZZZZZZZ",
        1,
        b"{}",
    );
    let found = store.check().unwrap();
    let problems: Vec<&str> = found.iter().map(|i| i.problem.as_str()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("its app does not exist")),
        "{found:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("its revision does not exist")),
        "{found:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains(&format!("also held by {id}"))),
        "{found:?}"
    );
}
