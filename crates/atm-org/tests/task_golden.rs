//! The golden task log. This file is the one place that states the body of
//! each `task.*` event kind; the log format is what these rows say.
//! Synthetic: hand-written, in an order the task policy allows. No harness.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_core::Money;
use atm_org::*;
use serde_json::{json, Value};
use support::role;
use tasks::{agent, file, tid, ts};

const TASK_KINDS: [&str; 10] = [
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
];

/// A work task `t12` goes from its brief to an accepted result, through a
/// plan stop judged by the review task `t20`. Every `task.*` kind appears.
fn task_rows() -> Vec<(u64, &'static str, Value)> {
    vec![
        (
            12,
            "task.created",
            json!({
                "title": "Task state machine",
                "kind": "work",
                "creator": "p/lead",
                "parent": null,
                "judges": null,
                "measure": {"kind": "mechanical", "command": "cargo test --workspace"},
                "budget": {
                    "cost": {"microdollars": 5000000},
                    "turns": 40,
                    "deadline": "2026-11-16T18:00:00Z"
                },
                "files": [{"path": "briefs/t1.md", "kind": "brief", "note": null}],
                "rests_on": null
            }),
        ),
        (
            13,
            "task.assigned",
            json!({"task_id": "t12", "agent_id": "p/build-1", "by": "p/lead"}),
        ),
        (
            14,
            "task.state_changed",
            json!({
                "task_id": "t12",
                "state": "blocked",
                "reason": "needs the decision types",
                "by": "p/build-1"
            }),
        ),
        (
            15,
            "task.reminded",
            json!({"task_id": "t12", "agent_id": "p/build-1"}),
        ),
        (
            16,
            "task.flagged",
            json!({"task_id": "t12", "to": "p/lead"}),
        ),
        (
            17,
            "task.attached",
            json!({
                "task_id": "t12",
                "file": {"path": "runs/suite.log", "kind": "evidence", "note": "sealed run"},
                "by": "p/build-1"
            }),
        ),
        (
            18,
            "task.submitted",
            json!({
                "task_id": "t12",
                "what": "plan",
                "files": [{"path": "briefs/t1-plan.md", "kind": "plan", "note": null}],
                "by": "p/build-1"
            }),
        ),
        (
            20,
            "task.created",
            json!({
                "title": "Review the plan of t12",
                "kind": "review",
                "creator": "p/lead",
                "parent": null,
                "judges": "t12",
                "measure": {"kind": "owner"},
                "budget": null,
                "files": [],
                "rests_on": 7
            }),
        ),
        (
            21,
            "task.review_started",
            json!({"task_id": "t12", "review_task": "t20", "by": "p/lead"}),
        ),
        (
            22,
            "task.assigned",
            json!({"task_id": "t20", "agent_id": "p/review-1", "by": "p/lead"}),
        ),
        (
            23,
            "task.submitted",
            json!({
                "task_id": "t20",
                "what": "result",
                "files": [{"path": "reviews/t12.md", "kind": "report", "note": null}],
                "by": "p/review-1"
            }),
        ),
        (
            24,
            "task.verdict",
            json!({
                "task_id": "t12",
                "verdict": "changes_requested",
                "review_task": "t20",
                "note": "two findings, see the review",
                "by": "p/lead"
            }),
        ),
        (
            25,
            "task.verdict",
            json!({
                "task_id": "t20",
                "verdict": "accepted",
                "review_task": null,
                "note": null,
                "by": "p/lead"
            }),
        ),
        (
            26,
            "task.submitted",
            json!({
                "task_id": "t12",
                "what": "result",
                "files": [{"path": "briefs/t1-report.md", "kind": "report", "note": null}],
                "by": "p/build-1"
            }),
        ),
        (
            27,
            "task.verdict",
            json!({
                "task_id": "t12",
                "verdict": "accepted",
                "review_task": null,
                "note": null,
                "by": "owner"
            }),
        ),
        (
            28,
            "task.cost_recorded",
            json!({
                "task_id": "t12",
                "record": {
                    "turns": 14,
                    "review_rounds": 2,
                    "changes_requested": 1,
                    "cost": {"microdollars": 2310000},
                    "peak_context_percent": 38,
                    "seconds_in_state": {"working": 5400, "in_review": 1200}
                }
            }),
        ),
    ]
}

/// The project and the three agents the task rows name, then the task rows.
fn golden_log() -> Vec<(u64, String, Value)> {
    let mut log = vec![(
        1,
        "project.registered".to_owned(),
        json!({"project_id": "p", "root": "/p"}),
    )];
    for (seq, name) in [(2, "lead"), (3, "build-1"), (4, "review-1")] {
        let mut role = role();
        role.name = name.into();
        let event = OrgEvent::AgentUpRequested {
            agent_id: agent(&format!("p/{name}")),
            role,
        };
        log.push((seq, event.kind().to_owned(), event.to_body()));
    }
    log.extend(
        task_rows()
            .into_iter()
            .map(|(seq, kind, body)| (seq, kind.to_owned(), body)),
    );
    log
}

fn fold(log: &[(u64, String, Value)], state: &mut OrgState, stats: &mut ReplayStats) {
    for (seq, kind, body) in log {
        stats
            .apply_parts(state, *seq, ts(*seq), kind, body)
            .unwrap_or_else(|error| panic!("{seq} {kind}: {error}"));
    }
}
fn folded(log: &[(u64, String, Value)]) -> OrgState {
    let mut state = OrgState::default();
    fold(log, &mut state, &mut ReplayStats::default());
    state
}
fn until(seq: u64) -> OrgState {
    let log = golden_log();
    let rows: Vec<_> = log.into_iter().filter(|row| row.0 <= seq).collect();
    folded(&rows)
}

#[test]
fn every_task_kind_has_a_golden_body_that_round_trips() {
    let rows = task_rows();
    let mut seen: Vec<&str> = rows.iter().map(|row| row.1).collect();
    seen.sort_unstable();
    seen.dedup();
    let mut expected = TASK_KINDS.to_vec();
    expected.sort_unstable();
    assert_eq!(seen, expected, "one golden row per task kind");
    for (seq, kind, body) in rows {
        let event = OrgEvent::from_parts(kind, &body)
            .unwrap_or_else(|error| panic!("{seq} {kind}: {error}"))
            .unwrap_or_else(|| panic!("{seq} {kind}: not a known kind"));
        assert_eq!(event.kind(), kind);
        assert!(body.is_object());
        assert_eq!(event.to_body(), body, "{seq} {kind}");
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
            Some(event)
        );
    }
}

#[test]
fn the_golden_log_folds_to_the_expected_state() {
    let log = golden_log();
    let mut state = OrgState::default();
    let mut stats = ReplayStats::default();
    fold(&log, &mut state, &mut stats);
    assert_eq!(stats.applied, log.len() as u64);
    assert_eq!(stats.skipped_unknown, 0);

    let lead: Address = "p/lead".parse().unwrap();
    let work = Task {
        id: tid(12),
        title: "Task state machine".into(),
        kind: TaskKind::Work,
        creator: lead.clone(),
        agent_id: Some(agent("p/build-1")),
        parent: None,
        judges: None,
        state: TaskState::Accepted,
        reason: None,
        submission: None,
        review_task: None,
        files: vec![
            file("briefs/t1.md", TaskFileKind::Brief),
            TaskFile {
                note: Some("sealed run".into()),
                ..file("runs/suite.log", TaskFileKind::Evidence)
            },
            file("briefs/t1-plan.md", TaskFileKind::Plan),
            file("briefs/t1-report.md", TaskFileKind::Report),
        ],
        measure: TaskMeasure::Mechanical {
            command: "cargo test --workspace".into(),
        },
        budget: Some(TaskBudget {
            cost: Some(Money::from_microdollars(5_000_000)),
            turns: Some(40),
            deadline: Some("2026-11-16T18:00:00Z".parse().unwrap()),
        }),
        rests_on: None,
        reminded_at: None,
        flagged_at: None,
        cost: Some(TaskCost {
            turns: Some(14),
            review_rounds: Some(2),
            changes_requested: Some(1),
            cost: Some(Money::from_microdollars(2_310_000)),
            peak_context_percent: Some(38),
            seconds_in_state: [(TaskState::Working, 5400), (TaskState::InReview, 1200)].into(),
        }),
        created_at: ts(12),
        changed_at: ts(27),
    };
    let review = Task {
        id: tid(20),
        title: "Review the plan of t12".into(),
        kind: TaskKind::Review,
        creator: lead,
        agent_id: Some(agent("p/review-1")),
        parent: None,
        judges: Some(tid(12)),
        state: TaskState::Accepted,
        reason: None,
        submission: None,
        review_task: None,
        files: vec![file("reviews/t12.md", TaskFileKind::Report)],
        measure: TaskMeasure::Owner,
        budget: None,
        rests_on: Some(7),
        reminded_at: None,
        flagged_at: None,
        cost: None,
        created_at: ts(20),
        changed_at: ts(25),
    };
    assert_eq!(state.task(&tid(12)), Some(&work));
    assert_eq!(state.task(&tid(20)), Some(&review));
    assert_eq!(state.tasks().collect::<Vec<_>>(), vec![&work, &review]);
    // Task events change tasks only.
    assert_eq!(state.agents().count(), 3);
    assert_eq!(state.messages().count(), 0);
}

#[test]
fn each_row_has_its_effect_on_the_way() {
    let at = |seq: u64| until(seq).task(&tid(12)).unwrap().clone();

    let open = at(12);
    assert_eq!((open.state, &open.agent_id), (TaskState::Open, &None));

    let assigned = at(13);
    assert_eq!(assigned.state, TaskState::Assigned);
    assert_eq!(assigned.agent_id, Some(agent("p/build-1")));
    assert_eq!(assigned.changed_at, ts(13));

    let flagged = at(16);
    assert_eq!(flagged.state, TaskState::Blocked);
    assert_eq!(flagged.reason.as_deref(), Some("needs the decision types"));
    assert_eq!(flagged.reminded_at, Some(ts(15)));
    assert_eq!(flagged.flagged_at, Some(ts(16)));
    assert_eq!(
        flagged.changed_at,
        ts(14),
        "reports do not change the state"
    );

    let attached = at(17);
    assert_eq!(attached.files.len(), 2);
    assert_eq!(attached.changed_at, ts(14));

    let submitted = at(18);
    assert_eq!(submitted.state, TaskState::Submitted);
    assert_eq!(submitted.submission, Some(TaskSubmission::Plan));
    assert_eq!(submitted.reason, None);
    assert_eq!((submitted.reminded_at, submitted.flagged_at), (None, None));
    assert_eq!(submitted.files.len(), 3);

    let in_review = at(21);
    assert_eq!(in_review.state, TaskState::InReview);
    assert_eq!(in_review.review_task, Some(tid(20)));
    assert_eq!(in_review.submission, Some(TaskSubmission::Plan));

    let sent_back = at(24);
    assert_eq!(sent_back.state, TaskState::ChangesRequested);
    assert_eq!(
        sent_back.reason.as_deref(),
        Some("two findings, see the review")
    );
    assert_eq!((sent_back.review_task, sent_back.submission), (None, None));
    assert_eq!(
        until(24).task(&tid(20)).unwrap().state,
        TaskState::Submitted,
        "one event changes one task"
    );
    assert_eq!(until(25).task(&tid(20)).unwrap().state, TaskState::Accepted);

    let accepted = at(27);
    assert_eq!(accepted.state, TaskState::Accepted);
    assert_eq!(accepted.cost, None);
}

#[test]
fn a_prefix_then_the_rest_equals_the_whole_at_every_cut() {
    let log = golden_log();
    let whole = folded(&log);
    assert_eq!(folded(&log), whole, "folding is deterministic");
    for cut in 0..=log.len() {
        let mut state = OrgState::default();
        let mut stats = ReplayStats::default();
        fold(&log[..cut], &mut state, &mut stats);
        fold(&log[cut..], &mut state, &mut stats);
        assert_eq!(state, whole, "cut {cut}");
        assert_eq!(stats.applied, log.len() as u64);
    }
}

#[test]
fn every_field_value_has_its_golden_text() {
    fn text<T: serde::Serialize>(value: T) -> Value {
        serde_json::to_value(value).unwrap()
    }
    assert_eq!(
        text([
            TaskKind::Work,
            TaskKind::Review,
            TaskKind::Plan,
            TaskKind::Errand,
            TaskKind::DriftCheck
        ]),
        json!(["work", "review", "plan", "errand", "drift_check"])
    );
    assert_eq!(
        text([
            TaskState::Open,
            TaskState::Assigned,
            TaskState::Working,
            TaskState::Waiting,
            TaskState::Blocked,
            TaskState::Submitted,
            TaskState::InReview,
            TaskState::Accepted,
            TaskState::ChangesRequested,
            TaskState::Cancelled
        ]),
        json!([
            "open",
            "assigned",
            "working",
            "waiting",
            "blocked",
            "submitted",
            "in_review",
            "accepted",
            "changes_requested",
            "cancelled"
        ])
    );
    assert_eq!(
        text([
            TaskFileKind::Brief,
            TaskFileKind::Report,
            TaskFileKind::Evidence,
            TaskFileKind::Plan,
            TaskFileKind::Contract
        ]),
        json!(["brief", "report", "evidence", "plan", "contract"])
    );
    assert_eq!(
        text([TaskSubmission::Plan, TaskSubmission::Result]),
        json!(["plan", "result"])
    );
    assert_eq!(
        text([TaskVerdict::Accepted, TaskVerdict::ChangesRequested]),
        json!(["accepted", "changes_requested"])
    );
    let measures = [
        (
            TaskMeasure::Mechanical {
                command: "make check".into(),
            },
            json!({"kind": "mechanical", "command": "make check"}),
        ),
        (
            TaskMeasure::Reviewed {
                rubric: "docs/rubric.md".into(),
            },
            json!({"kind": "reviewed", "rubric": "docs/rubric.md"}),
        ),
        (TaskMeasure::Owner, json!({"kind": "owner"})),
    ];
    for (measure, golden) in measures {
        assert_eq!(text(&measure), golden);
        assert_eq!(
            serde_json::from_value::<TaskMeasure>(golden).unwrap(),
            measure
        );
    }
    // A budget may hold any of its three parts; an empty record holds none.
    assert_eq!(
        text(TaskBudget {
            cost: None,
            turns: Some(3),
            deadline: None
        }),
        json!({"cost": null, "turns": 3, "deadline": null})
    );
    assert_eq!(
        text(TaskCost::default()),
        json!({
            "turns": null,
            "review_rounds": null,
            "changes_requested": null,
            "cost": null,
            "peak_context_percent": null,
            "seconds_in_state": {}
        })
    );
}

#[test]
fn a_broken_task_body_is_an_error_and_an_added_field_is_tolerated() {
    for (seq, kind, body) in task_rows() {
        let error = OrgEvent::from_parts(kind, &json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains(kind), "{error}");
        let event = OrgEvent::from_parts(kind, &body).unwrap();
        let mut wider = body.clone();
        wider["future"] = json!({"added": true});
        for nested in ["measure", "budget", "file", "record"] {
            if wider[nested].is_object() {
                wider[nested]["future"] = json!(1);
            }
        }
        if let Some(first) = wider.get_mut("files").and_then(|files| files.get_mut(0)) {
            first["future"] = json!("later");
        }
        assert_eq!(
            OrgEvent::from_parts(kind, &wider).unwrap(),
            event,
            "{seq} {kind}"
        );
    }
    // Ids and enumerations are checked, not passed through.
    for (kind, body) in [
        (
            "task.assigned",
            json!({"task_id": "t12", "agent_id": "p/bad name", "by": "p/lead"}),
        ),
        (
            "task.assigned",
            json!({"task_id": "", "agent_id": "p/build-1", "by": "p/lead"}),
        ),
        (
            "task.state_changed",
            json!({"task_id": "t12", "state": "merged", "reason": null, "by": "owner"}),
        ),
        (
            "task.verdict",
            json!({"task_id": "t12", "verdict": "maybe", "review_task": null, "note": null, "by": "owner"}),
        ),
    ] {
        assert!(OrgEvent::from_parts(kind, &body).is_err(), "{kind} {body}");
    }
}

#[test]
fn a_fact_about_a_missing_task_or_agent_leaves_the_state_unchanged() {
    let log = golden_log();
    let mut state = folded(&log);
    let before = state.clone();
    // Every kind but task.created names a task that must exist.
    for (seq, kind, body) in task_rows() {
        if kind == "task.created" {
            continue;
        }
        let mut body = body;
        body["task_id"] = json!("t999");
        let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
        assert_eq!(
            state.apply(100 + seq, ts(100), &event),
            Err(ApplyError::UnknownTask(tid(999))),
            "{kind}"
        );
    }
    let refused = [
        (
            "task.created",
            json!({"title": "x", "kind": "work", "creator": "p/lead", "parent": "t999",
                "judges": null, "measure": {"kind": "owner"}, "budget": null, "files": [],
                "rests_on": null}),
            ApplyError::UnknownTask(tid(999)),
        ),
        (
            "task.created",
            json!({"title": "x", "kind": "review", "creator": "p/lead", "parent": null,
                "judges": "t999", "measure": {"kind": "owner"}, "budget": null, "files": [],
                "rests_on": null}),
            ApplyError::UnknownTask(tid(999)),
        ),
        (
            "task.created",
            json!({"title": "x", "kind": "work", "creator": "p/ghost", "parent": null,
                "judges": null, "measure": {"kind": "owner"}, "budget": null, "files": [],
                "rests_on": null}),
            ApplyError::UnknownAgent(agent("p/ghost")),
        ),
        (
            "task.assigned",
            json!({"task_id": "t12", "agent_id": "p/ghost", "by": "p/lead"}),
            ApplyError::UnknownAgent(agent("p/ghost")),
        ),
        (
            "task.assigned",
            json!({"task_id": "t12", "agent_id": "p/build-1", "by": "p/ghost"}),
            ApplyError::UnknownAgent(agent("p/ghost")),
        ),
        (
            "task.review_started",
            json!({"task_id": "t12", "review_task": "t999", "by": "p/lead"}),
            ApplyError::UnknownTask(tid(999)),
        ),
        (
            "task.verdict",
            json!({"task_id": "t12", "verdict": "accepted", "review_task": "t999",
                "note": null, "by": "owner"}),
            ApplyError::UnknownTask(tid(999)),
        ),
        (
            "task.reminded",
            json!({"task_id": "t12", "agent_id": "p/ghost"}),
            ApplyError::UnknownAgent(agent("p/ghost")),
        ),
        (
            "task.flagged",
            json!({"task_id": "t12", "to": "p/ghost"}),
            ApplyError::UnknownAgent(agent("p/ghost")),
        ),
    ];
    for (kind, body, error) in refused {
        let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
        assert_eq!(
            state.apply(200, ts(200), &event),
            Err(error),
            "{kind} {body}"
        );
    }
    // A second task under a sequence number that already made one.
    let (_, kind, body) = task_rows().remove(0);
    let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
    assert_eq!(
        state.apply(12, ts(300), &event),
        Err(ApplyError::DuplicateTask(tid(12)))
    );
    assert_eq!(state, before);
}

#[test]
fn applying_a_fact_does_not_enforce_the_policy() {
    // A verdict on an open task is refused when asked for, and still folds
    // when it is already in a log: a later policy may have allowed it.
    let mut state = until(12);
    let event = OrgEvent::TaskVerdict(TaskVerdictGiven {
        task_id: tid(12),
        verdict: TaskVerdict::Accepted,
        review_task: None,
        note: None,
        by: Address::Owner,
    });
    state.apply(13, ts(13), &event).unwrap();
    assert_eq!(state.task(&tid(12)).unwrap().state, TaskState::Accepted);
}
