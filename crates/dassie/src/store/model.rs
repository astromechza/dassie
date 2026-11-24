//! Model tests: seeded random operations against the store and against a
//! plain `BTreeMap` that follows the same rules, compared after every step
//! (ADR 0016). This catches logic the store and `check` could both get wrong
//! the same way, such as a precondition checked against the wrong row.
//!
//! A failure names its seed. Rerun one with `DASSIE_MODEL_SEED=<seed>`.

use std::collections::BTreeMap;

use super::widget::{Colour, Phase, WidgetReads, WidgetSpec, WidgetStatus};
use super::{Error, Precondition, Reads, Revision, Store, WriteTxn};

const KEYS: [&str; 4] = ["a", "b", "c", "d"];
const LABELS: [Option<&str>; 3] = [None, Some("x"), Some("y")];
const STEPS: usize = 400;

/// xorshift64*: deterministic and dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

#[derive(Debug, Clone)]
enum Op {
    Create(&'static str, WidgetSpec),
    Replace(&'static str, WidgetSpec, Precondition),
    SetStatus(&'static str, WidgetStatus, Precondition, Precondition),
    Delete(&'static str, Precondition),
}

/// A result with errors reduced to their kind, so the model need not
/// predict messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok(Option<Revision>),
    NotFound,
    AlreadyExists,
    Conflict,
    Stale,
}

fn outcome(r: Result<Option<Revision>, Error>) -> Outcome {
    match r {
        Ok(rev) => Outcome::Ok(rev),
        Err(Error::NotFound { .. }) => Outcome::NotFound,
        Err(Error::AlreadyExists { .. }) => Outcome::AlreadyExists,
        Err(Error::Conflict { .. }) => Outcome::Conflict,
        Err(Error::Stale { .. }) => Outcome::Stale,
        Err(e) => panic!("unexpected store error: {e}"),
    }
}

fn run_op(w: &mut WriteTxn, op: &Op) -> Result<Option<Revision>, Error> {
    match op {
        Op::Create(k, spec) => w
            .create_widget(
                k,
                spec,
                &WidgetStatus {
                    phase: Phase::Pending,
                    observed: None,
                },
            )
            .map(Some),
        Op::Replace(k, spec, pre) => w.replace_widget_spec(k, spec, *pre).map(Some),
        Op::SetStatus(k, status, pre, spec_pre) => {
            w.set_widget_status(k, status, *pre, *spec_pre).map(Some)
        }
        Op::Delete(k, pre) => w.delete_widget(k, *pre).map(|()| None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    spec: WidgetSpec,
    spec_n: u64,
    status: WidgetStatus,
    status_n: u64,
}

#[derive(Debug, Clone)]
struct Model {
    epoch: u64,
    revision: u64,
    rows: BTreeMap<&'static str, Row>,
}

impl Model {
    fn matches(&self, pre: Precondition, actual: Option<u64>) -> bool {
        match pre {
            Precondition::Any => true,
            Precondition::Absent => actual.is_none(),
            Precondition::At(r) => r.epoch == self.epoch && Some(r.n) == actual,
        }
    }

    fn label_taken(&self, key: &str, spec: &WidgetSpec) -> bool {
        spec.label.is_some()
            && self
                .rows
                .iter()
                .any(|(k, row)| *k != key && row.spec.label == spec.label)
    }

    /// Applies `op` as part of a write that will commit at `n`.
    fn apply(&mut self, op: &Op, n: u64) -> Outcome {
        let rev = Revision {
            epoch: self.epoch,
            n,
        };
        match op {
            Op::Create(k, spec) => {
                if self.rows.contains_key(k) {
                    return Outcome::AlreadyExists;
                }
                if self.label_taken(k, spec) {
                    return Outcome::Conflict;
                }
                self.rows.insert(
                    k,
                    Row {
                        spec: spec.clone(),
                        spec_n: n,
                        status: WidgetStatus {
                            phase: Phase::Pending,
                            observed: None,
                        },
                        status_n: n,
                    },
                );
                Outcome::Ok(Some(rev))
            }
            Op::Replace(k, spec, pre) => {
                let Some(row) = self.rows.get(k) else {
                    return Outcome::NotFound;
                };
                if !self.matches(*pre, Some(row.spec_n)) {
                    return Outcome::Stale;
                }
                if self.label_taken(k, spec) {
                    return Outcome::Conflict;
                }
                let row = self.rows.get_mut(k).unwrap();
                row.spec = spec.clone();
                row.spec_n = n;
                Outcome::Ok(Some(rev))
            }
            Op::SetStatus(k, status, pre, spec_pre) => {
                let Some(row) = self.rows.get(k) else {
                    return Outcome::NotFound;
                };
                if !self.matches(*spec_pre, Some(row.spec_n))
                    || !self.matches(*pre, Some(row.status_n))
                {
                    return Outcome::Stale;
                }
                let row = self.rows.get_mut(k).unwrap();
                row.status = status.clone();
                row.status_n = n;
                Outcome::Ok(Some(rev))
            }
            Op::Delete(k, pre) => {
                let Some(row) = self.rows.get(k) else {
                    return Outcome::NotFound;
                };
                if !self.matches(*pre, Some(row.spec_n)) {
                    return Outcome::Stale;
                }
                self.rows.remove(k);
                Outcome::Ok(None)
            }
        }
    }

    /// Applies one write of `ops`: all of them, or none if any fails. Every
    /// op after a failure fails with the same error.
    fn write(&mut self, ops: &[Op]) -> Vec<Outcome> {
        let mut next = self.clone();
        let mut out = Vec::new();
        let mut failed = None;
        for op in ops {
            let o = failed.unwrap_or_else(|| next.apply(op, self.revision + 1));
            if !matches!(o, Outcome::Ok(_)) {
                failed.get_or_insert(o);
            }
            out.push(o);
        }
        if failed.is_none() {
            next.revision = self.revision + 1;
            *self = next;
        }
        out
    }
}

/// Random preconditions: mostly current revisions, often stale or foreign.
fn precondition(
    rng: &mut Rng,
    current: Option<u64>,
    model: &Model,
    issued: &[Revision],
) -> Precondition {
    match rng.below(6) {
        0 => Precondition::Any,
        1 => Precondition::Absent,
        2 if !issued.is_empty() => Precondition::At(rng.pick(issued)),
        3 => Precondition::At(Revision {
            epoch: model.epoch ^ 1,
            n: current.unwrap_or(1),
        }),
        _ => match current {
            Some(n) => Precondition::At(Revision {
                epoch: model.epoch,
                n,
            }),
            None => Precondition::Any,
        },
    }
}

fn random_op(rng: &mut Rng, model: &Model, issued: &[Revision]) -> Op {
    let key = rng.pick(&KEYS);
    let row = model.rows.get(key);
    let spec = WidgetSpec {
        size: u32::try_from(rng.below(10)).unwrap(),
        colour: rng.pick(&[Colour::Red, Colour::Blue]),
        label: rng.pick(&LABELS).map(Into::into),
    };
    match rng.below(4) {
        0 => Op::Create(key, spec),
        1 => Op::Replace(
            key,
            spec,
            precondition(rng, row.map(|r| r.spec_n), model, issued),
        ),
        2 => Op::SetStatus(
            key,
            WidgetStatus {
                phase: rng.pick(&[Phase::Pending, Phase::Ready]),
                observed: row.map(|r| r.spec_n),
            },
            precondition(rng, row.map(|r| r.status_n), model, issued),
            precondition(rng, row.map(|r| r.spec_n), model, issued),
        ),
        _ => Op::Delete(key, precondition(rng, row.map(|r| r.spec_n), model, issued)),
    }
}

fn run_seed(seed: u64) {
    let store = Store::in_memory().unwrap();
    let mut model = Model {
        epoch: store.epoch,
        revision: 0,
        rows: BTreeMap::new(),
    };
    let mut rng = Rng(seed.max(1));
    let mut issued = Vec::new();
    for step in 0..STEPS {
        let ops: Vec<Op> = (0..=rng.below(2))
            .map(|_| random_op(&mut rng, &model, &issued))
            .collect();
        let want = model.write(&ops);
        let mut got = Vec::new();
        let committed = store.write(|w| {
            for op in &ops {
                got.push(outcome(run_op(w, op)));
            }
            Ok::<_, Error>(())
        });
        let ctx = format!("seed {seed}, step {step}, ops {ops:?}");
        assert_eq!(got, want, "{ctx}");
        assert_eq!(
            committed.is_ok(),
            want.iter().all(|o| matches!(o, Outcome::Ok(_))),
            "{ctx}"
        );
        for o in &got {
            if let Outcome::Ok(Some(rev)) = o {
                issued.push(*rev);
            }
        }

        let rows: Vec<(String, Row)> = store
            .read(WidgetReads::widgets)
            .unwrap()
            .into_iter()
            .map(|(k, w)| {
                (
                    k,
                    Row {
                        spec: w.spec.value,
                        spec_n: w.spec.revision.n,
                        status: w.status.value,
                        status_n: w.status.revision.n,
                    },
                )
            })
            .collect();
        let want_rows: Vec<(String, Row)> = model
            .rows
            .iter()
            .map(|(k, r)| ((*k).to_owned(), r.clone()))
            .collect();
        assert_eq!(rows, want_rows, "{ctx}");
        assert_eq!(
            store.read(Reads::revision).unwrap().n,
            model.revision,
            "{ctx}"
        );
        assert_eq!(store.check().unwrap(), Vec::new(), "{ctx}");
    }
}

#[test]
fn the_store_matches_the_model() {
    if let Some(seed) = std::env::var("DASSIE_MODEL_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        run_seed(seed);
        return;
    }
    for seed in 1..=8 {
        run_seed(seed);
    }
}
