//! Property tests over random task histories. Synthetic: generated commands,
//! decided and applied one at a time; no harness, clock, daemon or tmux.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test generators/helpers only
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_org::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
};
use tasks::*;
use TaskState as S;

/// Every change of state the transition table allows, as (from, to).
#[rustfmt::skip]
const EDGES: [(S, &[S]); 10] = [
    (S::Open,             &[S::Assigned, S::Cancelled]),
    (S::Assigned,         &[S::Assigned, S::Working, S::Waiting, S::Blocked, S::Submitted, S::Open, S::Cancelled]),
    (S::Working,          &[S::Assigned, S::Waiting, S::Blocked, S::Submitted, S::Open, S::Cancelled]),
    (S::Waiting,          &[S::Assigned, S::Working, S::Waiting, S::Blocked, S::Submitted, S::Open, S::Cancelled]),
    (S::Blocked,          &[S::Assigned, S::Working, S::Waiting, S::Blocked, S::Submitted, S::Open, S::Cancelled]),
    (S::Submitted,        &[S::Working, S::InReview, S::Accepted, S::ChangesRequested, S::Cancelled]),
    (S::InReview,         &[S::Working, S::Accepted, S::ChangesRequested, S::Cancelled]),
    (S::ChangesRequested, &[S::Assigned, S::Working, S::Waiting, S::Blocked, S::Submitted, S::Open, S::Cancelled]),
    (S::Accepted,         &[]),
    (S::Cancelled,        &[]),
];
fn allowed(from: S, to: S) -> bool {
    EDGES
        .iter()
        .any(|(state, next)| *state == from && next.contains(&to))
}

#[derive(Debug, Clone)]
struct Step {
    op: usize,
    task: usize,
    other: usize,
    who: usize,
    variant: usize,
    /// Use the caller and the review task the move needs, so that histories
    /// go deep. Without it the step is an arbitrary request, mostly refused.
    fit: bool,
}
fn arb_step() -> impl Strategy<Value = Step> {
    (
        0usize..16,
        0usize..8,
        0usize..8,
        0usize..6,
        0usize..12,
        prop::bool::weighted(0.8),
    )
        .prop_map(|(op, task, other, who, variant, fit)| Step {
            op,
            task,
            other,
            who,
            variant,
            fit,
        })
}

const CALLERS: [&str; 6] = [LEAD, BUILD, OTHER, REVIEWER, "owner", "system"];
const SEATS: [&str; 3] = [BUILD, OTHER, REVIEWER];

impl Step {
    fn command(&self, state: &OrgState) -> TaskCommand {
        let all: Vec<&Task> = state.tasks().collect();
        let pick = |index: usize| all.get(index % all.len().max(1)).copied();
        let target = pick(self.task);
        let task_id = target.map_or_else(|| tid(999_999), |task| task.id.clone());
        let anyone = TaskActor {
            who: CALLERS[self.who].parse().unwrap(),
            lead: self.variant.is_multiple_of(5),
        };
        // The task's agent for its own moves; the owner for the others, who
        // has authority over every task and is never a task's agent.
        let own = match target.and_then(|task| task.agent_id.clone()) {
            Some(agent_id) if self.fit => TaskActor {
                who: Address::Agent(agent_id),
                lead: false,
            },
            _ => anyone.clone(),
        };
        let authority = if self.fit {
            by("owner")
        } else {
            anyone.clone()
        };
        let reviews: Vec<&Task> = all
            .iter()
            .copied()
            .filter(|task| task.judges.as_ref() == Some(&task_id))
            .collect();
        let handed_in = reviews
            .iter()
            .find(|review| review.state == S::Submitted)
            .map(|review| review.id.clone());
        let some_review = reviews
            .get(self.other % reviews.len().max(1))
            .map(|review| review.id.clone());
        let other_id = pick(self.other).map_or_else(|| tid(999_998), |task| task.id.clone());
        let reason = Some(format!("reason {}", self.variant % 2));
        let path = format!("file-{}.md", self.variant % 3);
        match self.op {
            0 | 1 => {
                let kind = [
                    TaskKind::Work,
                    TaskKind::Review,
                    TaskKind::Plan,
                    TaskKind::Errand,
                    TaskKind::DriftCheck,
                    TaskKind::Review,
                ][self.variant % 6];
                let measure =
                    [mechanical(), reviewed(), TaskMeasure::Owner][self.other % 3].clone();
                let mut body = new_task(kind, measure, CALLERS[self.who]);
                body.judges = match (kind == TaskKind::Review, self.fit) {
                    // Fitting: a review names a task, the other kinds do not.
                    (true, true) => target.map(|task| task.id.clone()),
                    (false, true) => None,
                    (_, false) => self.variant.is_multiple_of(2).then(|| other_id.clone()),
                };
                body.parent = self.variant.is_multiple_of(4).then_some(other_id);
                body.rests_on = self
                    .variant
                    .is_multiple_of(7)
                    .then_some(self.variant as u64);
                TaskCommand::Create(body)
            }
            2 => TaskCommand::Assign {
                task_id,
                agent_id: agent(SEATS[self.variant % 3]),
                by: authority,
            },
            3 | 4 => set(&task_id, S::Working, None, own),
            5 => TaskCommand::SetState {
                task_id,
                state: S::Waiting,
                reason,
                by: own,
            },
            6 => TaskCommand::SetState {
                task_id,
                state: S::Blocked,
                reason,
                by: own,
            },
            7 => {
                let state = if self.variant.is_multiple_of(3) {
                    S::Cancelled
                } else {
                    S::Open
                };
                set(&task_id, state, None, authority)
            }
            8 | 9 => TaskCommand::Submit {
                task_id,
                what: if self.variant.is_multiple_of(2) {
                    TaskSubmission::Plan
                } else {
                    TaskSubmission::Result
                },
                files: vec![file(&path, TaskFileKind::Report)],
                by: own,
            },
            10 => TaskCommand::StartReview {
                task_id,
                review_task: if self.fit {
                    some_review.unwrap_or(other_id)
                } else {
                    other_id
                },
                by: authority,
            },
            11 | 12 => TaskCommand::Verdict {
                task_id,
                verdict: if self.variant.is_multiple_of(3) {
                    TaskVerdict::ChangesRequested
                } else {
                    TaskVerdict::Accepted
                },
                review_task: match self.variant % 4 {
                    0 | 1 => None,
                    _ if self.fit => handed_in,
                    _ => Some(other_id),
                },
                note: self.variant.is_multiple_of(2).then(|| "a note".into()),
                by: authority,
            },
            13 => TaskCommand::Attach {
                task_id,
                file: file(&path, TaskFileKind::Evidence),
                by: anyone,
            },
            14 => {
                let agent_id = target
                    .and_then(|task| task.agent_id.clone())
                    .filter(|_| self.fit)
                    .unwrap_or_else(|| agent(SEATS[self.variant % 3]));
                if self.variant.is_multiple_of(2) {
                    TaskCommand::Reminded { task_id, agent_id }
                } else {
                    TaskCommand::Flagged {
                        task_id,
                        to: target
                            .map(|task| task.creator.clone())
                            .filter(|_| self.fit)
                            .unwrap_or(Address::Owner),
                    }
                }
            }
            _ => TaskCommand::RecordCost {
                task_id,
                record: TaskCost {
                    turns: Some(self.variant as u32),
                    ..TaskCost::default()
                },
            },
        }
    }
}

/// What must hold for every task after every committed event.
fn assert_task_invariants(state: &OrgState) {
    for task in state.tasks() {
        let held = matches!(
            task.state,
            S::Assigned | S::Working | S::Waiting | S::Blocked | S::ChangesRequested
        );
        match task.state {
            S::Open => assert_eq!(task.agent_id, None, "{task:?}"),
            S::Cancelled => {}
            _ => assert!(task.agent_id.is_some(), "{task:?}"),
        }
        if matches!(task.state, S::Waiting | S::Blocked) {
            assert!(task.reason.is_some(), "{task:?}");
        }
        assert_eq!(
            task.submission.is_some(),
            matches!(task.state, S::Submitted | S::InReview),
            "{task:?}"
        );
        assert_eq!(
            task.review_task.is_some(),
            task.state == S::InReview,
            "{task:?}"
        );
        if task.reminded_at.is_some() {
            assert!(held, "{task:?}");
        }
        if task.flagged_at.is_some() {
            assert!(task.reminded_at.is_some(), "{task:?}");
        }
        if task.cost.is_some() {
            assert_eq!(task.state, S::Accepted, "{task:?}");
        }
        assert_eq!(
            task.kind == TaskKind::Review,
            task.judges.is_some(),
            "{task:?}"
        );
        assert!(task.created_at <= task.changed_at, "{task:?}");
        assert_eq!(state.task(&task.id), Some(task));
    }
}

/// Every task moved along an edge of the table, or did not move at all.
fn assert_moves(before: &OrgState, after: &OrgState, command: &TaskCommand) {
    for old in before.tasks() {
        let new = after.task(&old.id).unwrap();
        if new.state != old.state || new.changed_at != old.changed_at {
            assert!(
                allowed(old.state, new.state),
                "{:?} -> {:?} by {command:?}",
                old.state,
                new.state
            );
        }
        // What a task was created with never changes.
        assert_eq!(
            (&new.title, new.kind, &new.creator, &new.parent, &new.judges),
            (&old.title, old.kind, &old.creator, &old.parent, &old.judges)
        );
        assert_eq!(
            (&new.measure, &new.budget, new.rests_on, new.created_at),
            (&old.measure, &old.budget, old.rests_on, old.created_at)
        );
        assert!(new.files.starts_with(&old.files), "files are only added");
    }
}

fn records(rig: &Rig) -> Vec<(u64, String, Value)> {
    rig.log
        .iter()
        .map(|(seq, event)| (*seq, event.kind().to_owned(), event.to_body()))
        .collect()
}
fn fold(log: &[(u64, String, Value)], state: &mut OrgState, stats: &mut ReplayStats) {
    for (seq, kind, body) in log {
        stats
            .apply_parts(state, *seq, ts(*seq), kind, body)
            .unwrap();
    }
}

#[test]
fn random_task_histories_follow_the_table_and_replay_to_the_same_state() {
    let states = RefCell::new(BTreeSet::new());
    let kinds = RefCell::new(BTreeSet::new());
    let refused = Cell::new(0u32);
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x7a51),
        failure_persistence: None, // the fixed seed reproduces the complete coverage run
        ..Config::default()
    });
    let inputs = (prop::collection::vec(arb_step(), 0..120), any::<usize>());
    runner
        .run(&inputs, |(steps, split)| {
            let mut rig = Rig::new();
            for step in steps {
                let command = step.command(&rig.state);
                let before = rig.state.clone();
                // `run` checks that deciding changed nothing and that every
                // decided event applies.
                match rig.run(command.clone()) {
                    Ok(events) => {
                        for event in &events {
                            kinds.borrow_mut().insert(event.kind());
                        }
                        if events.is_empty() {
                            prop_assert_eq!(&rig.state, &before);
                        }
                    }
                    Err(_) => {
                        refused.set(refused.get() + 1);
                        prop_assert_eq!(&rig.state, &before);
                    }
                }
                assert_moves(&before, &rig.state, &command);
                assert_task_invariants(&rig.state);
                states
                    .borrow_mut()
                    .extend(rig.state.tasks().map(|task| task.state));
            }
            let log = records(&rig);
            prop_assert!(log.windows(2).all(|pair| pair[0].0 < pair[1].0));
            let mut whole = OrgState::default();
            let mut stats = ReplayStats::default();
            fold(&log, &mut whole, &mut stats);
            prop_assert_eq!(&whole, &rig.state);
            prop_assert_eq!(stats.applied, log.len() as u64);
            let mut repeat = OrgState::default();
            let mut repeat_stats = ReplayStats::default();
            fold(&log, &mut repeat, &mut repeat_stats);
            prop_assert_eq!(&whole, &repeat);
            prop_assert_eq!(&stats, &repeat_stats);
            for cut in [0, log.len(), split % (log.len() + 1)] {
                let mut partial = OrgState::default();
                let mut partial_stats = ReplayStats::default();
                fold(&log[..cut], &mut partial, &mut partial_stats);
                fold(&log[cut..], &mut partial, &mut partial_stats);
                prop_assert_eq!(&whole, &partial);
                prop_assert_eq!(&stats, &partial_stats);
            }
            Ok(())
        })
        .unwrap();
    // The generator is not vacuous: it reached every state, wrote every kind
    // and was refused often.
    let states = states.into_inner();
    let refused = refused.get();
    println!("task histories (256 cases): states {states:?}, refused {refused}");
    assert_eq!(states.len(), 10, "{states:?}");
    let kinds = kinds.into_inner();
    for kind in [
        "task.created",
        "task.assigned",
        "task.state_changed",
        "task.submitted",
        "task.review_started",
        "task.verdict",
        "task.attached",
        "task.reminded",
        "task.flagged",
        "task.cost_recorded",
    ] {
        assert!(kinds.contains(kind), "no {kind} in any history");
    }
    assert!(refused > 1000, "only {refused} refusals");
}

#[test]
fn a_final_task_never_changes_state_again() {
    // Every move, by every caller, against an accepted and a cancelled task.
    for last in [S::Accepted, S::Cancelled] {
        let mut rig = Rig::new();
        let task = rig.work_task();
        rig.ok(assign(&task, BUILD, by(LEAD)));
        rig.ok(submit(
            &task,
            TaskSubmission::Result,
            "result.md",
            by(BUILD),
        ));
        if last == S::Accepted {
            rig.ok(verdict(&task, TaskVerdict::Accepted, None, by(LEAD)));
        } else {
            rig.ok(set(&task, S::Cancelled, None, by(LEAD)));
        }
        let review = rig.review_task(&task);
        for who in CALLERS {
            for lead in [false, true] {
                let who = TaskActor {
                    who: who.parse().unwrap(),
                    lead,
                };
                let mut moves = vec![
                    assign(&task, OTHER, who.clone()),
                    submit(&task, TaskSubmission::Result, "again.md", who.clone()),
                    start_review(&task, &review, who.clone()),
                    verdict(&task, TaskVerdict::Accepted, None, who.clone()),
                    verdict(&task, TaskVerdict::ChangesRequested, None, who.clone()),
                ];
                moves.extend(
                    [S::Open, S::Working, S::Waiting, S::Blocked, S::Cancelled]
                        .map(|state| set(&task, state, Some("why"), who.clone())),
                );
                for command in moves {
                    let _ = rig.run(command);
                    assert_eq!(rig.task(&task).state, last);
                }
            }
        }
    }
}
