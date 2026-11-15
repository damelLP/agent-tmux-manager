//! The task transition table, written out a second time as literal data, and
//! who may ask for each move. Synthetic: hand-written commands, no harness.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_org::*;
use tasks::*;
use Out::{No, Same, To};
use TaskState as S;

/// The ten states. The two that hold a submission appear once for a plan and
/// once for a result, because a verdict treats them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sit {
    Open,
    Assigned,
    Working,
    Waiting,
    Blocked,
    SubmittedPlan,
    SubmittedResult,
    InReviewPlan,
    InReviewResult,
    ChangesRequested,
    Accepted,
    Cancelled,
}
impl Sit {
    fn state(self) -> (S, Option<TaskSubmission>) {
        match self {
            Sit::Open => (S::Open, None),
            Sit::Assigned => (S::Assigned, None),
            Sit::Working => (S::Working, None),
            Sit::Waiting => (S::Waiting, None),
            Sit::Blocked => (S::Blocked, None),
            Sit::SubmittedPlan => (S::Submitted, Some(TaskSubmission::Plan)),
            Sit::SubmittedResult => (S::Submitted, Some(TaskSubmission::Result)),
            Sit::InReviewPlan => (S::InReview, Some(TaskSubmission::Plan)),
            Sit::InReviewResult => (S::InReview, Some(TaskSubmission::Result)),
            Sit::ChangesRequested => (S::ChangesRequested, None),
            Sit::Accepted => (S::Accepted, None),
            Sit::Cancelled => (S::Cancelled, None),
        }
    }
    fn plan(self) -> bool {
        self.state().1 == Some(TaskSubmission::Plan)
    }
}

/// The moves, in the table's column order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    /// To an agent the task does not have.
    Assign,
    Working,
    /// With a reason the task does not have.
    Waiting,
    Blocked,
    /// The submission the situation already made, or else a result.
    Submit,
    /// With the review task that is running, or else a new one of this task.
    StartReview,
    Accept,
    RequestChanges,
    Reopen,
    Cancel,
}
const MOVES: [Move; 10] = [
    Move::Assign,
    Move::Working,
    Move::Waiting,
    Move::Blocked,
    Move::Submit,
    Move::StartReview,
    Move::Accept,
    Move::RequestChanges,
    Move::Reopen,
    Move::Cancel,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    /// One event; the task ends in this state.
    To(S),
    /// Accepted, nothing written, nothing changed.
    Same,
    /// Refused because of the state the task is in; nothing changed.
    No,
}

// Columns: assign, working, waiting, blocked, submit, start review, accepted,
// changes requested, back to open, cancel.
#[rustfmt::skip]
const TABLE: [(Sit, [Out; 10]); 12] = [
    (Sit::Open,             [To(S::Assigned), No,             No,             No,             No,               No,              No,              No,                      Same,        To(S::Cancelled)]),
    (Sit::Assigned,         [To(S::Assigned), To(S::Working), To(S::Waiting), To(S::Blocked), To(S::Submitted), No,              No,              No,                      To(S::Open), To(S::Cancelled)]),
    (Sit::Working,          [To(S::Assigned), Same,           To(S::Waiting), To(S::Blocked), To(S::Submitted), No,              No,              No,                      To(S::Open), To(S::Cancelled)]),
    (Sit::Waiting,          [To(S::Assigned), To(S::Working), To(S::Waiting), To(S::Blocked), To(S::Submitted), No,              No,              No,                      To(S::Open), To(S::Cancelled)]),
    (Sit::Blocked,          [To(S::Assigned), To(S::Working), To(S::Waiting), To(S::Blocked), To(S::Submitted), No,              No,              No,                      To(S::Open), To(S::Cancelled)]),
    (Sit::SubmittedPlan,    [No,              To(S::Working), No,             No,             Same,             To(S::InReview), To(S::Working),  To(S::ChangesRequested), No,          To(S::Cancelled)]),
    (Sit::SubmittedResult,  [No,              To(S::Working), No,             No,             Same,             To(S::InReview), To(S::Accepted), To(S::ChangesRequested), No,          To(S::Cancelled)]),
    (Sit::InReviewPlan,     [No,              No,             No,             No,             No,               Same,            To(S::Working),  To(S::ChangesRequested), No,          To(S::Cancelled)]),
    (Sit::InReviewResult,   [No,              No,             No,             No,             No,               Same,            To(S::Accepted), To(S::ChangesRequested), No,          To(S::Cancelled)]),
    (Sit::ChangesRequested, [To(S::Assigned), To(S::Working), To(S::Waiting), To(S::Blocked), To(S::Submitted), No,              No,              Same,                    To(S::Open), To(S::Cancelled)]),
    (Sit::Accepted,         [No,              No,             No,             No,             No,               No,              Same,            No,                      No,          No]),
    (Sit::Cancelled,        [No,              No,             No,             No,             No,               No,              No,              No,                      No,          Same]),
];

struct Case {
    rig: Rig,
    task: TaskId,
    /// The review task that is running, in the two in-review situations.
    review: Option<TaskId>,
}
impl Case {
    fn task(&self) -> Task {
        self.rig.task(&self.task).clone()
    }
}

/// A work task of the lead's, measured by a command, brought into `sit` by
/// its own agent and the lead.
fn situation(sit: Sit) -> Case {
    let mut rig = Rig::new();
    let task = rig.work_task();
    let mut review = None;
    let build = || by(BUILD);
    let what = if sit.plan() {
        TaskSubmission::Plan
    } else {
        TaskSubmission::Result
    };
    let mut steps = vec![];
    if sit != Sit::Open {
        steps.push(assign(&task, BUILD, by(LEAD)));
    }
    match sit {
        Sit::Open | Sit::Assigned => {}
        Sit::Working => steps.push(set(&task, S::Working, None, build())),
        Sit::Waiting => steps.push(set(&task, S::Waiting, Some("first reason"), build())),
        Sit::Blocked => steps.push(set(&task, S::Blocked, Some("first reason"), build())),
        Sit::Cancelled => {
            steps.push(set(&task, S::Working, None, build()));
            steps.push(set(&task, S::Cancelled, None, by(LEAD)));
        }
        _ => {
            steps.push(set(&task, S::Working, None, build()));
            steps.push(submit(&task, what, "handed-in.md", build()));
        }
    }
    for step in steps {
        rig.ok(step);
    }
    match sit {
        Sit::InReviewPlan | Sit::InReviewResult => {
            let running = rig.review_task(&task);
            rig.ok(start_review(&task, &running, by(LEAD)));
            review = Some(running);
        }
        Sit::ChangesRequested => {
            rig.ok(verdict(
                &task,
                TaskVerdict::ChangesRequested,
                None,
                by(LEAD),
            ));
        }
        Sit::Accepted => {
            rig.ok(verdict(&task, TaskVerdict::Accepted, None, by(LEAD)));
        }
        _ => {}
    }
    let case = Case { rig, task, review };
    let reached = case.task();
    assert_eq!((reached.state, reached.submission), sit.state(), "{sit:?}");
    case
}

/// The command for one move. `who` replaces the caller the move needs: the
/// task's agent for its own moves, the creator for the others.
fn command(case: &mut Case, sit: Sit, mv: Move, who: Option<TaskActor>) -> TaskCommand {
    let task = case.task.clone();
    let own = who.clone().unwrap_or_else(|| by(BUILD));
    let authority = who.unwrap_or_else(|| by(LEAD));
    match mv {
        Move::Assign => assign(&task, OTHER, authority),
        Move::Working => set(&task, S::Working, None, own),
        Move::Waiting => set(&task, S::Waiting, Some("new reason"), own),
        Move::Blocked => set(&task, S::Blocked, Some("new reason"), own),
        Move::Submit => {
            let what = if sit.plan() {
                TaskSubmission::Plan
            } else {
                TaskSubmission::Result
            };
            submit(&task, what, "handed-in.md", own)
        }
        Move::StartReview => {
            let review = match &case.review {
                Some(running) => running.clone(),
                None => case.rig.review_task(&task),
            };
            start_review(&task, &review, authority)
        }
        Move::Accept => verdict(&task, TaskVerdict::Accepted, None, authority),
        Move::RequestChanges => verdict(&task, TaskVerdict::ChangesRequested, None, authority),
        Move::Reopen => set(&task, S::Open, None, authority),
        Move::Cancel => set(&task, S::Cancelled, None, authority),
    }
}

fn assert_state_refusal(result: Result<Vec<OrgEvent>, Refusal>, before: &Task, label: &str) {
    match result {
        Err(Refusal::Task(TaskRefusal::InvalidState { task_id, state, .. })) => {
            assert_eq!(
                (task_id, state),
                (before.id.clone(), before.state),
                "{label}"
            );
        }
        other => panic!("{label}: expected a state refusal, got {other:?}"),
    }
}

#[test]
fn every_cell_of_the_transition_table() {
    let mut cells = 0;
    for (sit, row) in TABLE {
        for (mv, expected) in MOVES.into_iter().zip(row) {
            let label = format!("{sit:?} x {mv:?}");
            let mut case = situation(sit);
            let command = command(&mut case, sit, mv, None);
            let before = case.task();
            let result = case.rig.run(command);
            let after = case.task();
            match expected {
                To(state) => {
                    let events = result.unwrap_or_else(|refusal| panic!("{label}: {refusal}"));
                    assert_eq!(events.len(), 1, "{label}");
                    assert_eq!(after.state, state, "{label}");
                    assert_eq!(after.changed_at, ts(case.rig.seq), "{label}");
                }
                Same => {
                    assert_eq!(result, Ok(vec![]), "{label}");
                    assert_eq!(after, before, "{label}");
                }
                No => {
                    assert_state_refusal(result, &before, &label);
                    assert_eq!(after, before, "{label}");
                }
            }
            cells += 1;
        }
    }
    assert_eq!(cells, 120);
}

#[test]
fn a_new_task_is_open_and_keeps_what_it_was_created_with() {
    let mut rig = Rig::new();
    let mut body = new_task(TaskKind::Errand, mechanical(), "owner");
    body.budget = Some(TaskBudget {
        cost: Some(atm_core::Money::from_microdollars(5_000_000)),
        turns: Some(40),
        deadline: Some(ts(1000)),
    });
    body.rests_on = Some(7);
    let parent = rig.work_task();
    body.parent = Some(parent.clone());
    let task = rig.create(body.clone());
    let created = rig.task(&task);
    assert_eq!(created.id, tid(rig.seq));
    assert_eq!(created.state, S::Open);
    assert_eq!(created.agent_id, None);
    assert_eq!(created.title, body.title);
    assert_eq!(created.kind, TaskKind::Errand);
    assert_eq!(created.creator, Address::Owner);
    assert_eq!(created.parent, Some(parent));
    assert_eq!(created.judges, None);
    assert_eq!(created.measure, body.measure);
    assert_eq!(created.budget, body.budget);
    assert_eq!(created.files, body.files);
    assert_eq!(created.rests_on, Some(7));
    assert_eq!(created.created_at, ts(rig.seq));
    assert_eq!(created.changed_at, ts(rig.seq));
    // Tasks are listed in the order they were created, not in text order.
    for _ in 0..9 {
        rig.work_task();
    }
    let ids: Vec<_> = rig.state.tasks().map(|task| task.id.clone()).collect();
    let mut sorted = ids.clone();
    sorted.sort_by_key(|id| {
        id.to_string()
            .trim_start_matches('t')
            .parse::<u64>()
            .unwrap()
    });
    assert_eq!(ids, sorted);
    assert_eq!(ids.len(), 11);
    // Only the canonical id finds a task.
    for text in [format!("t0{}", rig.seq), "t".into(), "unit-7".into()] {
        assert_eq!(rig.state.task(&text.parse().unwrap()), None, "{text}");
    }
}

#[test]
fn note_1_assigning_the_agent_it_has_writes_nothing_and_another_starts_over() {
    for sit in [
        Sit::Assigned,
        Sit::Working,
        Sit::Waiting,
        Sit::Blocked,
        Sit::ChangesRequested,
    ] {
        let mut case = situation(sit);
        let before = case.task();
        assert_eq!(
            case.rig.run(assign(&case.task, BUILD, by(LEAD))),
            Ok(vec![]),
            "{sit:?}"
        );
        assert_eq!(case.task(), before, "{sit:?}");
        let events = case.rig.ok(assign(&case.task, OTHER, by(LEAD)));
        assert_eq!(
            events,
            vec![OrgEvent::TaskAssigned(TaskAssigned {
                task_id: case.task.clone(),
                agent_id: agent(OTHER),
                by: LEAD.parse().unwrap(),
            })]
        );
        let after = case.task();
        assert_eq!(after.state, S::Assigned);
        assert_eq!(after.agent_id, Some(agent(OTHER)));
        assert_eq!(after.reason, None, "{sit:?}");
        // The task now answers to its new agent only.
        assert!(matches!(
            case.rig.run(set(&case.task, S::Working, None, by(BUILD))),
            Err(Refusal::Task(TaskRefusal::NotAllowed { .. }))
        ));
        case.rig.ok(set(&case.task, S::Working, None, by(OTHER)));
    }
}

#[test]
fn note_2_waiting_and_blocked_need_a_reason_and_repeat_only_a_new_one() {
    for (sit, state) in [(Sit::Waiting, S::Waiting), (Sit::Blocked, S::Blocked)] {
        let mut case = situation(sit);
        let before = case.task();
        assert_eq!(before.reason.as_deref(), Some("first reason"));
        assert_eq!(
            case.rig
                .run(set(&case.task, state, Some("first reason"), by(BUILD))),
            Ok(vec![])
        );
        assert_eq!(case.task(), before);
        let events = case
            .rig
            .ok(set(&case.task, state, Some("second reason"), by(BUILD)));
        assert_eq!(
            events,
            vec![OrgEvent::TaskStateChanged(TaskStateChanged {
                task_id: case.task.clone(),
                state,
                reason: Some("second reason".into()),
                by: BUILD.parse().unwrap(),
            })]
        );
        assert_eq!(case.task().reason.as_deref(), Some("second reason"));
        for missing in [None, Some("")] {
            assert!(
                matches!(
                    case.rig.run(set(&case.task, state, missing, by(BUILD))),
                    Err(Refusal::InvalidValue {
                        field: "reason",
                        ..
                    })
                ),
                "{state:?} {missing:?}"
            );
        }
    }
    // Leaving the state drops its reason.
    let mut case = situation(Sit::Blocked);
    case.rig.ok(set(&case.task, S::Working, None, by(BUILD)));
    assert_eq!(case.task().reason, None);
}

#[test]
fn note_3_the_agent_takes_its_submission_back() {
    for sit in [Sit::SubmittedPlan, Sit::SubmittedResult] {
        let mut case = situation(sit);
        case.rig.ok(set(&case.task, S::Working, None, by(BUILD)));
        let after = case.task();
        assert_eq!(after.state, S::Working);
        assert_eq!(after.submission, None);
        // The files it handed in stay attached.
        assert!(after.files.iter().any(|file| file.path == "handed-in.md"));
    }
}

#[test]
fn note_4_a_different_submission_waits_for_the_verdict() {
    let mut case = situation(Sit::SubmittedPlan);
    let before = case.task();
    for different in [
        submit(
            &case.task,
            TaskSubmission::Result,
            "handed-in.md",
            by(BUILD),
        ),
        submit(&case.task, TaskSubmission::Plan, "another.md", by(BUILD)),
    ] {
        let result = case.rig.run(different);
        assert_state_refusal(result, &before, "different submission");
        assert_eq!(case.task(), before);
    }
}

#[test]
fn submitting_attaches_the_files_once() {
    let mut case = situation(Sit::Working);
    let events = case.rig.ok(submit(
        &case.task,
        TaskSubmission::Result,
        "brief.md",
        by(BUILD),
    ));
    assert_eq!(
        events,
        vec![OrgEvent::TaskSubmitted(TaskSubmitted {
            task_id: case.task.clone(),
            what: TaskSubmission::Result,
            files: vec![file("brief.md", TaskFileKind::Report)],
            by: BUILD.parse().unwrap(),
        })]
    );
    case.rig.ok(set(&case.task, S::Working, None, by(BUILD)));
    case.rig.ok(submit(
        &case.task,
        TaskSubmission::Result,
        "brief.md",
        by(BUILD),
    ));
    let after = case.task();
    assert_eq!(
        after.files,
        vec![
            file("brief.md", TaskFileKind::Brief),
            file("brief.md", TaskFileKind::Report)
        ]
    );
    assert_eq!(after.submission, Some(TaskSubmission::Result));
    assert!(matches!(
        case.rig.run(TaskCommand::Submit {
            task_id: case.task.clone(),
            what: TaskSubmission::Result,
            files: vec![file("", TaskFileKind::Report)],
            by: by(BUILD),
        }),
        Err(Refusal::InvalidValue {
            field: "file path",
            ..
        })
    ));
}

#[test]
fn note_5_an_accepted_plan_goes_back_to_work_and_an_accepted_result_is_final() {
    for sit in [Sit::SubmittedPlan, Sit::InReviewPlan] {
        let mut case = situation(sit);
        let events = case
            .rig
            .ok(verdict(&case.task, TaskVerdict::Accepted, None, by(LEAD)));
        assert_eq!(events.len(), 1);
        let after = case.task();
        assert_eq!(after.state, S::Working, "{sit:?}");
        assert_eq!(after.submission, None);
        assert_eq!(after.review_task, None);
        // The work goes on to a result, which is then accepted for good.
        case.rig.ok(submit(
            &case.task,
            TaskSubmission::Result,
            "result.md",
            by(BUILD),
        ));
        case.rig
            .ok(verdict(&case.task, TaskVerdict::Accepted, None, by(LEAD)));
        assert_eq!(case.task().state, S::Accepted);
    }
}

#[test]
fn working_again_writes_nothing_even_when_the_task_carries_a_note() {
    // An approved plan leaves the task working with the verdict's note.
    let mut case = situation(Sit::SubmittedPlan);
    case.rig.ok(TaskCommand::Verdict {
        task_id: case.task.clone(),
        verdict: TaskVerdict::Accepted,
        review_task: None,
        note: Some("go ahead".into()),
        by: by(LEAD),
    });
    let before = case.task();
    assert_eq!(
        (before.state, before.reason.as_deref()),
        (S::Working, Some("go ahead"))
    );
    for reason in [None, Some("still at it")] {
        assert_eq!(
            case.rig.run(set(&case.task, S::Working, reason, by(BUILD))),
            Ok(vec![]),
            "{reason:?}"
        );
        assert_eq!(case.task(), before);
    }
}

#[test]
fn note_6_a_second_review_task_is_refused_while_one_runs() {
    let mut case = situation(Sit::InReviewResult);
    assert_eq!(case.task().review_task, case.review);
    let second = case.rig.review_task(&case.task);
    let before = case.task();
    let result = case.rig.run(start_review(&case.task, &second, by(LEAD)));
    assert_state_refusal(result, &before, "second review");
    assert_eq!(case.task(), before);
    // The review task that is running stays `same`, even once it is over.
    let running = case.review.clone().unwrap();
    case.rig.ok(set(&running, S::Cancelled, None, by(LEAD)));
    assert_eq!(
        case.rig.run(start_review(&case.task, &running, by(LEAD))),
        Ok(vec![])
    );
    assert_eq!(case.task(), before);
}

#[test]
fn back_to_open_removes_the_agent_and_cancel_keeps_a_reason() {
    let mut case = situation(Sit::Working);
    let events = case.rig.ok(set(
        &case.task,
        S::Open,
        Some("the seat went away"),
        by(LEAD),
    ));
    assert_eq!(
        events,
        vec![OrgEvent::TaskStateChanged(TaskStateChanged {
            task_id: case.task.clone(),
            state: S::Open,
            reason: Some("the seat went away".into()),
            by: LEAD.parse().unwrap(),
        })]
    );
    let open = case.task();
    assert_eq!(open.state, S::Open);
    assert_eq!(open.agent_id, None);
    assert_eq!(open.reason.as_deref(), Some("the seat went away"));
    case.rig
        .ok(set(&case.task, S::Cancelled, Some("superseded"), by(LEAD)));
    let cancelled = case.task();
    assert_eq!(cancelled.state, S::Cancelled);
    assert_eq!(cancelled.reason.as_deref(), Some("superseded"));
    assert!(matches!(
        case.rig
            .run(set(&case.task, S::Cancelled, Some(""), by(LEAD))),
        Err(Refusal::InvalidValue {
            field: "reason",
            ..
        })
    ));
}

#[test]
fn states_with_their_own_move_cannot_be_set_directly() {
    let mut case = situation(Sit::Working);
    let before = case.task();
    for state in [
        S::Assigned,
        S::Submitted,
        S::InReview,
        S::Accepted,
        S::ChangesRequested,
    ] {
        for who in [by(BUILD), by(LEAD), by("owner")] {
            assert_eq!(
                case.rig.run(set(&case.task, state, None, who)),
                Err(Refusal::Task(TaskRefusal::NotSettable { state })),
                "{state:?}"
            );
        }
    }
    assert_eq!(case.task(), before);
}

/// Callers, in the column order of the rows below: the task's agent, another
/// agent, the creator, an agent the daemon marks as the lead, the owner, the
/// daemon itself.
fn callers() -> [TaskActor; 6] {
    [
        by(BUILD),
        by(OTHER),
        by(LEAD),
        as_lead(BOSS),
        by("owner"),
        by("system"),
    ]
}
const ONLY_ITS_AGENT: [bool; 6] = [true, false, false, false, false, false];
const AUTHORITY: [bool; 6] = [false, false, true, true, true, false];

#[test]
fn who_may_ask_for_each_move() {
    // Each move from a situation in which the right caller is not refused.
    let rows = [
        (Move::Assign, Sit::Assigned, AUTHORITY),
        (Move::Working, Sit::Assigned, ONLY_ITS_AGENT),
        (Move::Waiting, Sit::Working, ONLY_ITS_AGENT),
        (Move::Blocked, Sit::Working, ONLY_ITS_AGENT),
        (Move::Submit, Sit::Working, ONLY_ITS_AGENT),
        (Move::StartReview, Sit::SubmittedResult, AUTHORITY),
        (Move::Accept, Sit::SubmittedResult, AUTHORITY),
        (Move::RequestChanges, Sit::SubmittedResult, AUTHORITY),
        (Move::Reopen, Sit::Working, AUTHORITY),
        (Move::Cancel, Sit::Working, AUTHORITY),
    ];
    assert_eq!(rows.map(|row| row.0), MOVES);
    for (mv, sit, allowed) in rows {
        for (who, allowed) in callers().into_iter().zip(allowed) {
            let label = format!("{mv:?} by {}", who.who);
            let mut case = situation(sit);
            let command = command(&mut case, sit, mv, Some(who.clone()));
            let before = case.task();
            let result = case.rig.run(command);
            if allowed {
                let events = result.unwrap_or_else(|refusal| panic!("{label}: {refusal}"));
                assert_eq!(events.len(), 1, "{label}");
            } else {
                match result {
                    Err(Refusal::Task(TaskRefusal::NotAllowed {
                        task_id,
                        who: refused,
                        ..
                    })) => assert_eq!((task_id, refused), (case.task.clone(), who.who), "{label}"),
                    other => panic!("{label}: expected not allowed, got {other:?}"),
                }
                assert_eq!(case.task(), before, "{label}");
            }
        }
    }
}

#[test]
fn a_caller_without_the_right_is_refused_even_where_nothing_would_change() {
    // Working on a working task writes nothing for its agent. Anyone else must
    // still hear a refusal, not a silent success.
    let mut case = situation(Sit::Working);
    for who in [by(OTHER), by(LEAD), by("owner")] {
        assert!(matches!(
            case.rig.run(set(&case.task, S::Working, None, who)),
            Err(Refusal::Task(TaskRefusal::NotAllowed { .. }))
        ));
    }
    let mut case = situation(Sit::Accepted);
    assert!(matches!(
        case.rig
            .run(verdict(&case.task, TaskVerdict::Accepted, None, by(OTHER))),
        Err(Refusal::Task(TaskRefusal::NotAllowed { .. }))
    ));
}

#[test]
fn anyone_may_create_a_task_and_attach_a_file() {
    for who in callers() {
        let mut rig = Rig::new();
        let mut body = new_task(TaskKind::Work, mechanical(), LEAD);
        body.creator = who.who.clone();
        let task = rig.create(body);
        assert_eq!(rig.task(&task).creator, who.who);
        let evidence = TaskFile {
            note: Some("sealed run".into()),
            ..file("run.log", TaskFileKind::Evidence)
        };
        let attach = TaskCommand::Attach {
            task_id: task.clone(),
            file: evidence.clone(),
            by: who.clone(),
        };
        assert_eq!(
            rig.ok(attach.clone()),
            vec![OrgEvent::TaskAttached(TaskAttached {
                task_id: task.clone(),
                file: evidence.clone(),
                by: who.who.clone(),
            })]
        );
        let before = rig.task(&task).clone();
        assert_eq!(before.files.last(), Some(&evidence));
        assert_eq!(before.state, S::Open);
        // The same file again writes nothing.
        assert_eq!(rig.run(attach), Ok(vec![]));
        assert_eq!(rig.task(&task), &before);
    }
}

#[test]
fn a_file_can_be_attached_in_every_state() {
    for (sit, _) in TABLE {
        let mut case = situation(sit);
        let before = case.task();
        case.rig.ok(TaskCommand::Attach {
            task_id: case.task.clone(),
            file: file("contract.md", TaskFileKind::Contract),
            by: by(OTHER),
        });
        let after = case.task();
        assert_eq!(after.files.len(), before.files.len() + 1, "{sit:?}");
        assert_eq!(
            Task {
                files: before.files.clone(),
                ..after
            },
            before,
            "{sit:?}: attaching changed more than the files"
        );
    }
    let mut case = situation(Sit::Open);
    assert!(matches!(
        case.rig.run(TaskCommand::Attach {
            task_id: case.task.clone(),
            file: file("", TaskFileKind::Evidence),
            by: by(OTHER),
        }),
        Err(Refusal::InvalidValue {
            field: "file path",
            ..
        })
    ));
}

#[test]
fn rule_7_nobody_gives_the_verdict_on_a_task_it_is_the_agent_of() {
    // The agent created the task and took it itself: it has the creator's
    // authority to assign, and still cannot judge its own work.
    let mut rig = Rig::new();
    let task = rig.create(new_task(TaskKind::Work, mechanical(), BUILD));
    rig.ok(assign(&task, BUILD, by(BUILD)));
    rig.ok(submit(
        &task,
        TaskSubmission::Result,
        "result.md",
        by(BUILD),
    ));
    let before = rig.task(&task).clone();
    for who in [by(BUILD), as_lead(BUILD)] {
        for given in [TaskVerdict::Accepted, TaskVerdict::ChangesRequested] {
            match rig.run(verdict(&task, given, None, who.clone())) {
                Err(Refusal::Task(TaskRefusal::NotAllowed {
                    who: refused,
                    operation,
                    ..
                })) => {
                    assert_eq!(refused, who.who);
                    assert!(operation.contains("own"), "{operation}");
                }
                other => panic!("expected not allowed, got {other:?}"),
            }
        }
    }
    assert_eq!(rig.task(&task), &before);
    rig.ok(verdict(&task, TaskVerdict::Accepted, None, by("owner")));
    assert_eq!(rig.task(&task).state, S::Accepted);
}

#[test]
fn rule_8_a_verdict_that_names_a_review_task_moves_the_judged_task_and_closes_the_review() {
    for (given, ends) in [
        (TaskVerdict::ChangesRequested, S::ChangesRequested),
        (TaskVerdict::Accepted, S::Accepted),
    ] {
        for in_review in [false, true] {
            let mut case = situation(Sit::SubmittedResult);
            let review = case.rig.submitted_review(&case.task);
            if in_review {
                case.rig.ok(start_review(&case.task, &review, by(LEAD)));
            }
            let events = case.rig.ok(TaskCommand::Verdict {
                task_id: case.task.clone(),
                verdict: given,
                review_task: Some(review.clone()),
                note: Some("see the review".into()),
                by: by(LEAD),
            });
            assert_eq!(
                events,
                vec![
                    OrgEvent::TaskVerdict(TaskVerdictGiven {
                        task_id: case.task.clone(),
                        verdict: given,
                        review_task: Some(review.clone()),
                        note: Some("see the review".into()),
                        by: LEAD.parse().unwrap(),
                    }),
                    OrgEvent::TaskVerdict(TaskVerdictGiven {
                        task_id: review.clone(),
                        verdict: TaskVerdict::Accepted,
                        review_task: None,
                        note: None,
                        by: LEAD.parse().unwrap(),
                    }),
                ]
            );
            let judged = case.task();
            assert_eq!(judged.state, ends);
            assert_eq!(judged.reason.as_deref(), Some("see the review"));
            assert_eq!(judged.review_task, None);
            assert_eq!(judged.submission, None);
            assert_eq!(case.rig.task(&review).state, S::Accepted);
        }
    }
}

#[test]
fn probe_review_handed_in_as_plan_is_closed_as() {
    for (given, ends) in [
        (TaskVerdict::Accepted, S::Accepted),
        (TaskVerdict::ChangesRequested, S::ChangesRequested),
    ] {
        for in_review in [false, true] {
            let (mut rig, task) = handed_in(mechanical(), TaskSubmission::Result);
            let review = rig.review_task(&task);
            rig.ok(assign(&review, REVIEWER, by(LEAD)));
            rig.ok(submit(
                &review,
                TaskSubmission::Plan,
                "review-plan.md",
                by(REVIEWER),
            ));
            if in_review {
                rig.ok(start_review(&task, &review, by(LEAD)));
            }
            let before = rig.state.clone();
            let log = rig.log.clone();
            let seq = rig.seq;
            for who in [by(LEAD), by("owner")] {
                assert_eq!(
                    rig.run(verdict(&task, given, Some(&review), who)),
                    Err(Refusal::Task(TaskRefusal::ReviewNotSubmitted {
                        review_task: review.clone(),
                        state: S::Submitted,
                    }))
                );
                assert_eq!(rig.state, before);
                assert_eq!(rig.log, log);
                assert_eq!(rig.seq, seq);
            }
            // A review's plan stop is legal, but its plan is not the review result.
            rig.ok(verdict(&review, TaskVerdict::Accepted, None, by(LEAD)));
            assert_eq!(rig.task(&review).state, S::Working);
            rig.ok(submit(
                &review,
                TaskSubmission::Result,
                "review.md",
                by(REVIEWER),
            ));
            assert_eq!(
                rig.ok(verdict(&task, given, Some(&review), by(LEAD))).len(),
                2
            );
            assert_eq!(rig.task(&task).state, ends);
            assert_eq!(rig.task(&review).state, S::Accepted);
        }
    }
}

#[test]
fn rule_8_the_named_task_must_be_a_handed_in_review_of_this_task() {
    let mut case = situation(Sit::SubmittedResult);
    let task = case.task.clone();
    let before = case.task();
    let give =
        |review: &TaskId| verdict(&task, TaskVerdict::ChangesRequested, Some(review), by(LEAD));

    // Not a review at all.
    let plain = case.rig.work_task();
    // A handed-in review, but of another task.
    let elsewhere = case.rig.work_task();
    let foreign = case.rig.submitted_review(&elsewhere);
    for wrong in [&plain, &foreign] {
        assert_eq!(
            case.rig.run(give(wrong)),
            Err(Refusal::Task(TaskRefusal::NotReviewOf {
                task_id: task.clone(),
                review_task: wrong.clone(),
            }))
        );
    }
    // A review of this task that its reviewer has not handed in.
    let unfinished = case.rig.review_task(&task);
    case.rig.ok(assign(&unfinished, REVIEWER, by(LEAD)));
    case.rig
        .ok(set(&unfinished, S::Working, None, by(REVIEWER)));
    assert_eq!(
        case.rig.run(give(&unfinished)),
        Err(Refusal::Task(TaskRefusal::ReviewNotSubmitted {
            review_task: unfinished.clone(),
            state: S::Working,
        }))
    );
    // A task that does not exist.
    assert_eq!(
        case.rig.run(give(&tid(999_999))),
        Err(Refusal::Invariant(ApplyError::UnknownTask(tid(999_999))))
    );
    assert_eq!(case.task(), before);
    assert_eq!(case.rig.task(&unfinished).state, S::Working);
}

#[test]
fn rule_8_a_review_handed_in_by_the_judged_tasks_own_agent_does_not_count() {
    // The agent makes a review of its own task, takes it and hands it in. It
    // may do all three with a task it created; the review must not count.
    let (mut rig, task) = handed_in(reviewed(), TaskSubmission::Result);
    let mut body = new_task(TaskKind::Review, mechanical(), BUILD);
    body.judges = Some(task.clone());
    let own = rig.create(body);
    rig.ok(assign(&own, BUILD, by(BUILD)));
    rig.ok(submit(
        &own,
        TaskSubmission::Result,
        "self-review.md",
        by(BUILD),
    ));
    let before = rig.task(&task).clone();
    for who in [by(LEAD), as_lead(BOSS), by("owner")] {
        for given in [TaskVerdict::Accepted, TaskVerdict::ChangesRequested] {
            assert_eq!(
                rig.run(verdict(&task, given, Some(&own), who.clone())),
                Err(Refusal::Task(TaskRefusal::OwnReview {
                    task_id: task.clone(),
                    review_task: own.clone(),
                })),
                "{given:?} by {}",
                who.who
            );
        }
    }
    assert_eq!(rig.task(&task), &before);
    assert_eq!(rig.task(&own).state, S::Submitted);
    // With no review that counts, the reviewed measure still refuses the lead.
    assert_measure_refusal(
        rig.run(verdict(&task, TaskVerdict::Accepted, None, by(LEAD))),
        &task,
    );
    // A review that another agent handed in counts.
    let review = rig.submitted_review(&task);
    rig.ok(verdict(
        &task,
        TaskVerdict::Accepted,
        Some(&review),
        by(LEAD),
    ));
    assert_eq!(rig.task(&task).state, S::Accepted);
    assert_eq!(rig.task(&own).state, S::Submitted);
}

#[test]
fn starting_a_review_needs_a_live_review_task_of_this_task() {
    let mut case = situation(Sit::SubmittedResult);
    let task = case.task.clone();
    let before = case.task();
    let plain = case.rig.work_task();
    let elsewhere = case.rig.work_task();
    let foreign = case.rig.review_task(&elsewhere);
    for wrong in [&plain, &foreign] {
        assert_eq!(
            case.rig.run(start_review(&task, wrong, by(LEAD))),
            Err(Refusal::Task(TaskRefusal::NotReviewOf {
                task_id: task.clone(),
                review_task: wrong.clone(),
            }))
        );
    }
    let finished = case.rig.review_task(&task);
    case.rig.ok(set(&finished, S::Cancelled, None, by(LEAD)));
    assert_eq!(
        case.rig.run(start_review(&task, &finished, by(LEAD))),
        Err(Refusal::Task(TaskRefusal::InvalidState {
            task_id: finished.clone(),
            state: S::Cancelled,
            operation: "start a review with",
        }))
    );
    assert_eq!(
        case.rig.run(start_review(&task, &tid(999_999), by(LEAD))),
        Err(Refusal::Invariant(ApplyError::UnknownTask(tid(999_999))))
    );
    assert_eq!(case.task(), before);

    let review = case.rig.review_task(&task);
    assert_eq!(
        case.rig.ok(start_review(&task, &review, by(LEAD))),
        vec![OrgEvent::TaskReviewStarted(TaskReviewStarted {
            task_id: task.clone(),
            review_task: review.clone(),
            by: LEAD.parse().unwrap(),
        })]
    );
    let judged = case.task();
    assert_eq!(judged.state, S::InReview);
    assert_eq!(judged.review_task, Some(review));
    assert_eq!(judged.submission, Some(TaskSubmission::Result));
}

/// A result handed in on a task with this measure, created by the lead.
fn handed_in(measure: TaskMeasure, what: TaskSubmission) -> (Rig, TaskId) {
    let mut rig = Rig::new();
    let task = rig.create(new_task(TaskKind::Work, measure, LEAD));
    rig.ok(assign(&task, BUILD, by(LEAD)));
    rig.ok(submit(&task, what, "handed-in.md", by(BUILD)));
    (rig, task)
}
fn assert_measure_refusal(result: Result<Vec<OrgEvent>, Refusal>, task: &TaskId) {
    match result {
        Err(Refusal::Task(TaskRefusal::Measure { task_id, .. })) => assert_eq!(&task_id, task),
        other => panic!("expected a measure refusal, got {other:?}"),
    }
}

#[test]
fn rule_9_a_mechanical_measure_lets_authority_accept() {
    for who in [by(LEAD), as_lead(BOSS), by("owner")] {
        let (mut rig, task) = handed_in(mechanical(), TaskSubmission::Result);
        rig.ok(verdict(&task, TaskVerdict::Accepted, None, who));
        assert_eq!(rig.task(&task).state, S::Accepted);
    }
}

#[test]
fn rule_9_a_reviewed_measure_needs_a_handed_in_review_unless_the_owner_accepts() {
    for who in [by(LEAD), as_lead(BOSS)] {
        let (mut rig, task) = handed_in(reviewed(), TaskSubmission::Result);
        let before = rig.task(&task).clone();
        assert_measure_refusal(
            rig.run(verdict(&task, TaskVerdict::Accepted, None, who.clone())),
            &task,
        );
        assert_eq!(rig.task(&task), &before);
        let review = rig.submitted_review(&task);
        rig.ok(verdict(&task, TaskVerdict::Accepted, Some(&review), who));
        assert_eq!(rig.task(&task).state, S::Accepted);
    }
    let (mut rig, task) = handed_in(reviewed(), TaskSubmission::Result);
    rig.ok(verdict(&task, TaskVerdict::Accepted, None, by("owner")));
    assert_eq!(rig.task(&task).state, S::Accepted);
}

#[test]
fn rule_9_the_owners_judgement_is_the_owners_alone() {
    for who in [by(LEAD), as_lead(BOSS)] {
        let (mut rig, task) = handed_in(TaskMeasure::Owner, TaskSubmission::Result);
        let review = rig.submitted_review(&task);
        let before = rig.task(&task).clone();
        for named in [None, Some(&review)] {
            assert_measure_refusal(
                rig.run(verdict(&task, TaskVerdict::Accepted, named, who.clone())),
                &task,
            );
        }
        assert_eq!(rig.task(&task), &before);
        assert_eq!(rig.task(&review).state, S::Submitted);
    }
    let (mut rig, task) = handed_in(TaskMeasure::Owner, TaskSubmission::Result);
    rig.ok(verdict(&task, TaskVerdict::Accepted, None, by("owner")));
    assert_eq!(rig.task(&task).state, S::Accepted);
}

#[test]
fn rule_9_limits_only_the_acceptance_of_a_result() {
    for measure in [reviewed(), TaskMeasure::Owner] {
        // Approving a plan is not acceptance of the work.
        let (mut rig, task) = handed_in(measure.clone(), TaskSubmission::Plan);
        rig.ok(verdict(&task, TaskVerdict::Accepted, None, by(LEAD)));
        assert_eq!(rig.task(&task).state, S::Working);
        // Anyone with authority may send a result back.
        let (mut rig, task) = handed_in(measure, TaskSubmission::Result);
        rig.ok(verdict(
            &task,
            TaskVerdict::ChangesRequested,
            None,
            by(LEAD),
        ));
        assert_eq!(rig.task(&task).state, S::ChangesRequested);
    }
}

#[test]
fn create_refuses_what_a_task_cannot_be() {
    let mut rig = Rig::new();
    let existing = rig.work_task();
    let count = rig.state.tasks().count();
    let base = || new_task(TaskKind::Work, mechanical(), LEAD);
    let invalid = |field: &'static str, body: TaskCreated, rig: &mut Rig| match rig
        .run(TaskCommand::Create(body))
    {
        Err(Refusal::InvalidValue { field: found, .. }) => assert_eq!(found, field),
        other => panic!("{field}: expected an invalid value, got {other:?}"),
    };
    invalid(
        "title",
        TaskCreated {
            title: String::new(),
            ..base()
        },
        &mut rig,
    );
    invalid(
        "file path",
        TaskCreated {
            files: vec![file("", TaskFileKind::Brief)],
            ..base()
        },
        &mut rig,
    );
    invalid(
        "measure command",
        TaskCreated {
            measure: TaskMeasure::Mechanical {
                command: String::new(),
            },
            ..base()
        },
        &mut rig,
    );
    invalid(
        "measure rubric",
        TaskCreated {
            measure: TaskMeasure::Reviewed {
                rubric: String::new(),
            },
            ..base()
        },
        &mut rig,
    );
    // A review names the task it judges; no other kind does.
    for (kind, judges) in [
        (TaskKind::Review, None),
        (TaskKind::Work, Some(existing.clone())),
        (TaskKind::Plan, Some(existing.clone())),
        (TaskKind::Errand, Some(existing.clone())),
        (TaskKind::DriftCheck, Some(existing.clone())),
    ] {
        assert_eq!(
            rig.run(TaskCommand::Create(TaskCreated {
                kind,
                judges,
                ..base()
            })),
            Err(Refusal::Task(TaskRefusal::Judges { kind })),
            "{kind:?}"
        );
    }
    // References must exist.
    for body in [
        TaskCreated {
            parent: Some(tid(999_999)),
            ..base()
        },
        TaskCreated {
            kind: TaskKind::Review,
            judges: Some(tid(999_999)),
            ..base()
        },
    ] {
        assert_eq!(
            rig.run(TaskCommand::Create(body)),
            Err(Refusal::Invariant(ApplyError::UnknownTask(tid(999_999))))
        );
    }
    assert_eq!(
        rig.run(TaskCommand::Create(new_task(
            TaskKind::Work,
            mechanical(),
            "p/missing"
        ))),
        Err(Refusal::Invariant(ApplyError::UnknownAgent(agent(
            "p/missing"
        ))))
    );
    assert_eq!(rig.state.tasks().count(), count);
    // Every kind can be created, a review with the task it judges.
    for kind in [
        TaskKind::Work,
        TaskKind::Plan,
        TaskKind::Errand,
        TaskKind::DriftCheck,
    ] {
        let task = rig.create(TaskCreated { kind, ..base() });
        assert_eq!(rig.task(&task).kind, kind);
    }
    let review = rig.review_task(&existing);
    assert_eq!(rig.task(&review).judges, Some(existing));
}

#[test]
fn unknown_tasks_and_agents_are_refused_before_policy() {
    let mut rig = Rig::new();
    let task = rig.work_task();
    let missing = tid(999_999);
    let unknown_task = Err(Refusal::Invariant(ApplyError::UnknownTask(missing.clone())));
    for command in [
        assign(&missing, BUILD, by(LEAD)),
        set(&missing, S::Working, None, by(BUILD)),
        submit(&missing, TaskSubmission::Result, "x.md", by(BUILD)),
        verdict(&missing, TaskVerdict::Accepted, None, by(LEAD)),
        TaskCommand::Attach {
            task_id: missing.clone(),
            file: file("x.md", TaskFileKind::Evidence),
            by: by(LEAD),
        },
        TaskCommand::Reminded {
            task_id: missing.clone(),
            agent_id: agent(BUILD),
        },
        TaskCommand::Flagged {
            task_id: missing.clone(),
            to: LEAD.parse().unwrap(),
        },
        TaskCommand::RecordCost {
            task_id: missing.clone(),
            record: TaskCost::default(),
        },
    ] {
        assert_eq!(rig.run(command), unknown_task);
    }
    let unknown_agent = Err(Refusal::Invariant(ApplyError::UnknownAgent(agent(
        "p/missing",
    ))));
    assert_eq!(rig.run(assign(&task, "p/missing", by(LEAD))), unknown_agent);
    assert_eq!(
        rig.run(assign(&task, BUILD, by("p/missing"))),
        unknown_agent
    );
    assert_eq!(rig.task(&task).state, S::Open);
}

const HELD: [Sit; 5] = [
    Sit::Assigned,
    Sit::Working,
    Sit::Waiting,
    Sit::Blocked,
    Sit::ChangesRequested,
];

#[test]
fn a_reminder_then_a_flag_are_recorded_once_per_state() {
    for sit in HELD {
        let mut case = situation(sit);
        let reminded = TaskCommand::Reminded {
            task_id: case.task.clone(),
            agent_id: agent(BUILD),
        };
        let flagged = TaskCommand::Flagged {
            task_id: case.task.clone(),
            to: LEAD.parse().unwrap(),
        };
        // No flag before a reminder.
        assert_eq!(
            case.rig.run(flagged.clone()),
            Err(Refusal::Task(TaskRefusal::NotReminded {
                task_id: case.task.clone()
            })),
            "{sit:?}"
        );
        assert_eq!(
            case.rig.ok(reminded.clone()),
            vec![OrgEvent::TaskReminded(TaskReminded {
                task_id: case.task.clone(),
                agent_id: agent(BUILD),
            })]
        );
        let reminded_at = ts(case.rig.seq);
        assert_eq!(case.task().reminded_at, Some(reminded_at));
        assert_eq!(case.rig.run(reminded.clone()), Ok(vec![]));
        assert_eq!(
            case.rig.ok(flagged.clone()),
            vec![OrgEvent::TaskFlagged(TaskFlagged {
                task_id: case.task.clone(),
                to: LEAD.parse().unwrap(),
            })]
        );
        let after = case.task();
        assert_eq!(after.reminded_at, Some(reminded_at));
        assert_eq!(after.flagged_at, Some(ts(case.rig.seq)));
        assert_eq!(after.state, sit.state().0, "a report changed the state");
        assert_eq!(case.rig.run(flagged.clone()), Ok(vec![]));
        assert_eq!(case.task(), after);
        // The next change of state starts over.
        case.rig.ok(set(
            &case.task,
            S::Blocked,
            Some("something else"),
            by(BUILD),
        ));
        let moved = case.task();
        assert_eq!((moved.reminded_at, moved.flagged_at), (None, None));
        assert_eq!(case.rig.ok(reminded).len(), 1);
    }
}

#[test]
fn reports_are_refused_where_they_make_no_sense() {
    for (sit, _) in TABLE {
        let mut case = situation(sit);
        let before = case.task();
        if !HELD.contains(&sit) {
            // Nobody is expected to act on the task, so nobody is reminded.
            let result = case.rig.run(TaskCommand::Reminded {
                task_id: case.task.clone(),
                agent_id: agent(BUILD),
            });
            assert_state_refusal(result, &before, &format!("{sit:?} reminded"));
            let result = case.rig.run(TaskCommand::Flagged {
                task_id: case.task.clone(),
                to: LEAD.parse().unwrap(),
            });
            assert_state_refusal(result, &before, &format!("{sit:?} flagged"));
        }
        if sit != Sit::Accepted {
            let result = case.rig.run(TaskCommand::RecordCost {
                task_id: case.task.clone(),
                record: TaskCost::default(),
            });
            assert_state_refusal(result, &before, &format!("{sit:?} cost"));
        }
        assert_eq!(case.task(), before, "{sit:?}");
    }
    // The reminder names the task's agent, the flag its creator.
    let mut case = situation(Sit::Working);
    assert_eq!(
        case.rig.run(TaskCommand::Reminded {
            task_id: case.task.clone(),
            agent_id: agent(OTHER),
        }),
        Err(Refusal::Task(TaskRefusal::NotAllowed {
            task_id: case.task.clone(),
            who: OTHER.parse().unwrap(),
            operation: "be reminded of",
        }))
    );
    case.rig.ok(TaskCommand::Reminded {
        task_id: case.task.clone(),
        agent_id: agent(BUILD),
    });
    assert_eq!(
        case.rig.run(TaskCommand::Flagged {
            task_id: case.task.clone(),
            to: OTHER.parse().unwrap(),
        }),
        Err(Refusal::Task(TaskRefusal::NotAllowed {
            task_id: case.task.clone(),
            who: OTHER.parse().unwrap(),
            operation: "be told about",
        }))
    );
}

#[test]
fn the_cost_record_is_kept_on_an_accepted_task() {
    let mut case = situation(Sit::Accepted);
    let record = TaskCost {
        turns: Some(14),
        review_rounds: Some(2),
        changes_requested: Some(1),
        cost: Some(atm_core::Money::from_microdollars(2_310_000)),
        peak_context_percent: Some(38),
        seconds_in_state: [(S::Working, 5400), (S::InReview, 1200)].into(),
    };
    let command = TaskCommand::RecordCost {
        task_id: case.task.clone(),
        record: record.clone(),
    };
    assert_eq!(
        case.rig.ok(command.clone()),
        vec![OrgEvent::TaskCostRecorded(TaskCostRecorded {
            task_id: case.task.clone(),
            record: record.clone(),
        })]
    );
    assert_eq!(case.task().cost, Some(record.clone()));
    assert_eq!(case.task().state, S::Accepted);
    assert_eq!(case.rig.run(command), Ok(vec![]));
    // A later, fuller record replaces it.
    let fuller = TaskCost {
        turns: Some(15),
        ..record
    };
    case.rig.ok(TaskCommand::RecordCost {
        task_id: case.task.clone(),
        record: fuller.clone(),
    });
    assert_eq!(case.task().cost, Some(fuller));
}
