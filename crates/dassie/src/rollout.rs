//! ADR 0014's two reconcile rules for one app, as a pure function: rows and
//! unit readiness in, the next actions out. The daemon's pass does the I/O
//! and the store writes around it (the reconcile ADR); this module only
//! decides.

/// One of the app's instances, as the pass read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The ULID, which sorts oldest first.
    pub(crate) id: String,
    /// The hash of the revision it runs.
    pub(crate) revision: String,
    /// Its unit is `active` (ADR 0012).
    pub(crate) ready: bool,
    /// Marked for deletion: not counted.
    pub(crate) deleting: bool,
}

/// What one pass should do for the app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// New instances of the app's revision to create.
    pub(crate) create: u32,
    /// Instances to mark for deletion, in the order chosen.
    pub(crate) remove: Vec<String>,
}

/// The app's counts, for its status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) current: u32,
    pub(crate) old: u32,
    pub(crate) deleting: u32,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).expect("an app has at most 32768 instances")
}

/// The status counts: counted instances of the app's revision, counted ones
/// of older revisions, and those being deleted.
pub(crate) fn counts(revision: &str, seen: &[Seen]) -> Counts {
    let live = |current: bool| {
        count(
            seen.iter()
                .filter(|s| !s.deleting && (s.revision == revision) == current)
                .count(),
        )
    };
    Counts {
        current: live(true),
        old: live(false),
        deleting: count(seen.iter().filter(|s| s.deleting).count()),
    }
}

/// Applies both rules for an app with `replicas` and current `revision`.
/// `seen` is every instance of the app, oldest first.
pub(crate) fn plan(replicas: u32, revision: &str, seen: &[Seen]) -> Plan {
    let mut live: Vec<&Seen> = seen.iter().filter(|s| !s.deleting).collect();
    let current = count(live.iter().filter(|s| s.revision == revision).count());
    // Rule 1: one at a time while the revision is short and the app is
    // below R + 1, counting only instances not being deleted.
    let total = count(live.len());
    let create = replicas
        .saturating_sub(current)
        .min((replicas + 1).saturating_sub(total));
    // Rule 2: while more than R are counted, mark the first that may go.
    let mut remove = Vec::new();
    while count(live.len()) > replicas {
        let Some(pos) = next_to_go(replicas, revision, &live) else {
            break;
        };
        remove.push(live.remove(pos).id.clone());
    }
    Plan { create, remove }
}

/// The instance rule 2 removes next, if any may go: older revisions before
/// the app's, then not ready before ready, then oldest first.
fn next_to_go(replicas: u32, revision: &str, live: &[&Seen]) -> Option<usize> {
    let mut order: Vec<usize> = (0..live.len()).collect();
    // Stable sort keeps the oldest-first order within each group.
    order.sort_by_key(|&i| (live[i].revision == revision, live[i].ready));
    order
        .into_iter()
        .find(|&i| may_go(replicas, revision, live, live[i]))
}

/// Rule 2's test for one counted instance against the others counted with
/// it: not ready, or more than R ready; and if it runs the app's revision,
/// that revision has more than R.
fn may_go(replicas: u32, revision: &str, live: &[&Seen], s: &Seen) -> bool {
    let ready = count(live.iter().filter(|x| x.ready).count());
    let current = count(live.iter().filter(|x| x.revision == revision).count());
    (!s.ready || ready > replicas) && (s.revision != revision || current > replicas)
}

/// The check just before stopping an instance rule 2 marked: with the app's
/// spec and unit states read again, would rule 2 still remove it? `seen`
/// includes it, still marked; the mark is withdrawn if this says no.
pub(crate) fn still_goes(replicas: u32, revision: &str, seen: &[Seen], id: &str) -> bool {
    let live: Vec<&Seen> = seen.iter().filter(|s| !s.deleting || s.id == id).collect();
    let Some(s) = live.iter().find(|s| s.id == id) else {
        return false;
    };
    count(live.len()) > replicas && may_go(replicas, revision, &live, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW: &str = "new";
    const OLD: &str = "old";

    /// `ids` are given oldest first.
    fn seen(rows: &[(&str, &str, bool)]) -> Vec<Seen> {
        rows.iter()
            .map(|(id, revision, ready)| Seen {
                id: (*id).into(),
                revision: (*revision).into(),
                ready: *ready,
                deleting: false,
            })
            .collect()
    }

    #[test]
    fn a_rollout_replaces_one_at_a_time() {
        // R = 3, three old instances ready, change of revision.
        let mut s = seen(&[("a", OLD, true), ("b", OLD, true), ("c", OLD, true)]);
        assert_eq!(
            plan(3, NEW, &s),
            Plan {
                create: 1,
                remove: vec![]
            }
        );
        // The new one is starting: nothing else happens.
        s.push(seen(&[("d", NEW, false)]).remove(0));
        assert_eq!(plan(3, NEW, &s), Plan::default());
        // It is ready: the oldest old one goes.
        s[3].ready = true;
        assert_eq!(
            plan(3, NEW, &s),
            Plan {
                create: 0,
                remove: vec!["a".into()]
            }
        );
        // Marked, it stops counting, so the next new one starts.
        s[0].deleting = true;
        assert_eq!(
            plan(3, NEW, &s),
            Plan {
                create: 1,
                remove: vec![]
            }
        );
        assert_eq!(
            counts(NEW, &s),
            Counts {
                current: 1,
                old: 2,
                deleting: 1
            }
        );
    }

    #[test]
    fn scaling() {
        // Up: every missing instance at once.
        let s = seen(&[("a", NEW, true)]);
        assert_eq!(
            plan(4, NEW, &s),
            Plan {
                create: 3,
                remove: vec![]
            }
        );
        // Down: not ready first, then oldest.
        let s = seen(&[("a", NEW, true), ("b", NEW, false), ("c", NEW, true)]);
        assert_eq!(
            plan(1, NEW, &s),
            Plan {
                create: 0,
                remove: vec!["b".into(), "a".into()]
            }
        );
        // To zero: everything.
        assert_eq!(plan(0, NEW, &s).remove.len(), 3);
    }

    #[test]
    fn a_revision_that_never_becomes_ready_keeps_the_old_serving() {
        let s = seen(&[("a", OLD, true), ("b", NEW, false)]);
        assert_eq!(plan(1, NEW, &s), Plan::default());
    }

    #[test]
    fn a_surplus_failed_instance_goes_while_the_old_one_serves() {
        // R = 1: one ready old instance, two failed new ones.
        let s = seen(&[("a", OLD, true), ("b", NEW, false), ("c", NEW, false)]);
        assert_eq!(
            plan(1, NEW, &s),
            Plan {
                create: 0,
                remove: vec!["b".into()]
            }
        );
    }

    #[test]
    fn unready_old_instances_go_without_waiting() {
        // R = 2: two unready old instances, one ready current.
        let s = seen(&[("a", OLD, false), ("b", OLD, false), ("c", NEW, true)]);
        assert_eq!(
            plan(2, NEW, &s),
            Plan {
                create: 0,
                remove: vec!["a".into()]
            }
        );
    }

    #[test]
    fn a_failed_current_instance_is_left_failed() {
        let s = seen(&[("a", NEW, false), ("b", NEW, true)]);
        assert_eq!(plan(2, NEW, &s), Plan::default());
    }

    #[test]
    fn the_pre_stop_check() {
        let mut s = seen(&[("a", OLD, true), ("b", NEW, true)]);
        s[0].deleting = true;
        assert!(still_goes(1, NEW, &s, "a"));
        // The new one failed since: the ready old one stays.
        s[1].ready = false;
        assert!(!still_goes(1, NEW, &s, "a"));
        // R went up since: no longer surplus.
        s[1].ready = true;
        assert!(!still_goes(2, NEW, &s, "a"));
        // An unready instance goes even if few are ready.
        s[0].ready = false;
        s[1].ready = false;
        assert!(still_goes(1, NEW, &s, "a"));
        assert!(!still_goes(1, NEW, &s, "missing"));
    }
}
