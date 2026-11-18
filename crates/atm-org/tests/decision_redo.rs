//! Synthetic overrule histories; no real harness payloads.
use atm_org::*;
mod support;
use support::decision_world::World;

fn change(w: &mut World, id: &TaskId, state: TaskState) {
    w.event(OrgEvent::TaskStateChanged(TaskStateChanged {
        task_id: id.clone(),
        state,
        reason: None,
        by: Address::Owner,
    }));
}

// Break caught: final dependent work remains accepted after the decision changes.
#[test]
fn one_overrule_fact_reopens_all_frozen_work() {
    let mut w = World::new();
    let d = w.request("alpha/lead", true);
    let root = w.create(Some(d), None, None);
    let child = w.create(None, Some(root.clone()), None);
    let review = w.create(None, None, Some(root.clone()));
    let cancelled = w.create(Some(d), None, None);
    let unstarted = w.create(Some(d), None, None);
    let unrelated = w.create(None, None, None);
    for t in [&root, &child, &review, &cancelled] {
        w.assign(t, "beta/lead").unwrap();
    }
    change(&mut w, &root, TaskState::Accepted);
    w.set(&child, TaskState::Open);
    w.set(&cancelled, TaskState::Cancelled);
    w.away();
    w.take(d);
    let events = w.answer(d, "large").unwrap();
    assert_eq!(
        events.len(),
        1,
        "one durable fact must contain the entire reset"
    );
    assert!(matches!(
        events.first(),
        Some(OrgEvent::DecisionOverruled(_))
    ));
    for t in [&root, &child, &review] {
        let task = w.state.task(t).unwrap();
        assert_eq!(
            task.state,
            TaskState::Open,
            "dependent {t} must be invalidated"
        );
        assert_eq!(task.agent_id, None);
        assert_eq!(task.submission, None);
        assert_eq!(task.review_task, None);
        assert_eq!(task.cost, None);
        assert_eq!(
            task.reason.as_deref(),
            Some(format!("Decision {d} overruled; redo against large.").as_str())
        );
        assert!(w.state.task_was_assigned(t));
    }
    assert_eq!(
        w.state.task(&cancelled).unwrap().state,
        TaskState::Cancelled
    );
    for t in [&unstarted, &unrelated] {
        assert_eq!(w.state.task(t).unwrap().reason, None);
    }
    assert_eq!(w.replay(), w.state);
}

// Break caught: validation mutates a prefix, or duplicate redo references reset twice.
#[test]
fn bad_redo_reference_changes_nothing() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let t = w.create(Some(d), None, None);
    w.assign(&t, "beta/lead").unwrap();
    w.away();
    w.take(d);
    let answer = DecisionResolution {
        decision_id: d,
        option: "large".into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    };
    for ids in [
        vec![t.clone(), "t999999".parse().unwrap()],
        vec![t.clone(), t.clone()],
    ] {
        let mut event: DecisionResolutionEvent = answer.clone().into();
        event.redo_tasks = Some(ids);
        let before = w.state.clone();
        assert!(w
            .state
            .apply(999, w.now, &OrgEvent::DecisionOverruled(event))
            .is_err());
        assert_eq!(w.state, before);
    }
}

// Break caught: a retry invalidates newly assigned work or credits a plan as rework.
#[test]
fn redo_requires_reassigned_accepted_result_and_exact_repeat_does_not_reset() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let t = w.create(Some(d), None, None);
    w.assign(&t, "beta/lead").unwrap();
    w.away();
    w.take(d);
    w.answer(d, "large").unwrap();
    assert_eq!(
        w.state.decision_redo(d).unwrap().required,
        Some(vec![t.clone()])
    );
    assert!(w.state.decision_redo(d).unwrap().completed.is_empty());
    let actor = TaskActor {
        who: "beta/lead".parse().unwrap(),
        manager_of: None,
    };
    assert!(w
        .step(OrgCommand::Task(TaskCommand::Submit {
            task_id: t.clone(),
            what: TaskSubmission::Result,
            files: vec![],
            by: actor
        }))
        .is_err());
    assert!(w
        .step(OrgCommand::Task(TaskCommand::Verdict {
            task_id: t.clone(),
            verdict: TaskVerdict::Accepted,
            review_task: None,
            note: None,
            by: support::decision_world::owner()
        }))
        .is_err());
    w.assign(&t, "gamma/chief").unwrap();
    let before = w.state.clone();
    assert!(w.answer(d, "large").unwrap().is_empty());
    assert_eq!(w.state, before);
    for what in [TaskSubmission::Plan, TaskSubmission::Result] {
        w.event(OrgEvent::TaskSubmitted(TaskSubmitted {
            task_id: t.clone(),
            what,
            files: vec![],
            by: "gamma/chief".parse().unwrap(),
        }));
        w.event(OrgEvent::TaskVerdict(TaskVerdictGiven {
            task_id: t.clone(),
            verdict: TaskVerdict::Accepted,
            review_task: None,
            note: None,
            by: Address::Owner,
        }));
        assert_eq!(
            w.state.decision_redo(d).unwrap().completed.len(),
            usize::from(what == TaskSubmission::Result)
        );
    }
    assert_eq!(w.replay(), w.state);
}

// Break caught: missing redo metadata is interpreted as a new destructive action.
#[test]
fn legacy_numbers_are_unrecorded_and_keep_caller() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let t = w.create(Some(d), None, None);
    w.assign(&t, "beta/lead").unwrap();
    change(&mut w, &t, TaskState::Accepted);
    w.away();
    w.take(d);
    let mut answer: DecisionResolutionEvent = DecisionResolution {
        decision_id: d,
        option: "large".into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    }
    .into();
    answer.caller_pane = Some("%9".into());
    w.event(OrgEvent::DecisionOverruled(answer));
    assert_eq!(w.state.task(&t).unwrap().state, TaskState::Accepted);
    let progress = w.state.decision_redo(d).unwrap();
    assert_eq!(progress.required, None);
    assert_eq!(progress.caller_pane.as_deref(), Some("%9"));
    assert!(w.state.redo_notices().is_empty());
    assert_eq!(w.replay(), w.state);
}

// Break caught: one accepted redo satisfies only the latest overrule or is counted twice.
#[test]
fn shared_redo_credits_each_overrule_once_and_cancellation_is_not_success() {
    let mut w = World::new();
    let a = w.request("alpha/lead", false);
    let b = w.request("beta/lead", false);
    let parent = w.create(Some(a), None, None);
    let child = w.create(Some(b), Some(parent.clone()), None);
    w.assign(&parent, "alpha/lead").unwrap();
    w.assign(&child, "gamma/chief").unwrap();
    w.away();
    w.take(a);
    w.take(b);
    w.answer(a, "large").unwrap();
    w.answer(b, "large").unwrap();
    w.set(&parent, TaskState::Cancelled);
    assert_eq!(
        w.state
            .decision_redo(a)
            .unwrap()
            .required
            .as_ref()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(w.state.decision_redo(a).unwrap().cancelled.len(), 1);
    w.assign(&child, "gamma/chief").unwrap();
    for _ in 0..2 {
        w.event(OrgEvent::TaskSubmitted(TaskSubmitted {
            task_id: child.clone(),
            what: TaskSubmission::Result,
            files: vec![],
            by: "gamma/chief".parse().unwrap(),
        }));
        w.event(OrgEvent::TaskVerdict(TaskVerdictGiven {
            task_id: child.clone(),
            verdict: TaskVerdict::Accepted,
            review_task: None,
            note: None,
            by: Address::Owner,
        }));
    }
    for d in [a, b] {
        assert_eq!(
            w.state
                .decision_redo(d)
                .unwrap()
                .completed
                .iter()
                .collect::<Vec<_>>(),
            vec![&child]
        );
    }
    assert_eq!(w.replay(), w.state);
}

// Break caught: a later overrule makes the same task both completed and cancelled.
#[test]
fn accepted_redo_is_not_cancelled_for_an_earlier_overrule() {
    let mut w = World::new();
    let a = w.request("alpha/lead", false);
    let b = w.request("beta/lead", false);
    let parent = w.create(Some(a), None, None);
    let child = w.create(Some(b), Some(parent.clone()), None);
    w.assign(&parent, "alpha/lead").unwrap();
    w.assign(&child, "gamma/chief").unwrap();
    w.away();
    w.take(a);
    w.take(b);
    w.answer(a, "large").unwrap();
    w.assign(&child, "gamma/chief").unwrap();
    w.run(OrgCommand::Task(TaskCommand::Submit {
        task_id: child.clone(),
        what: TaskSubmission::Result,
        files: vec![],
        by: TaskActor {
            who: "gamma/chief".parse().unwrap(),
            manager_of: None,
        },
    }));
    w.run(OrgCommand::Task(TaskCommand::Verdict {
        task_id: child.clone(),
        verdict: TaskVerdict::Accepted,
        review_task: None,
        note: None,
        by: support::decision_world::owner(),
    }));
    assert_eq!(w.state.task(&child).unwrap().state, TaskState::Accepted);
    w.answer(b, "large").unwrap();
    assert_eq!(w.state.task(&child).unwrap().state, TaskState::Open);
    w.set(&child, TaskState::Cancelled);

    let earlier = w.state.decision_redo(a).unwrap();
    assert_eq!(earlier.required, Some(vec![parent, child.clone()]));
    assert_eq!(earlier.completed, [child.clone()].into());
    assert!(earlier.cancelled.is_empty(), "already accepted under a");
    let later = w.state.decision_redo(b).unwrap();
    assert_eq!(later.required, Some(vec![child.clone()]));
    assert!(later.completed.is_empty());
    assert_eq!(later.cancelled, [child].into());

    let back = w.run(OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }));
    w.run(OrgCommand::Owner(OwnerCommand::CreateDigest {
        by: Address::Owner,
        back_seq: Some(back),
    }));
    let numbers: Vec<_> = w
        .state
        .return_digest()
        .unwrap()
        .rows
        .iter()
        .filter_map(|row| match row {
            DigestRow::OverruleNumber {
                decision_id,
                completed,
                required,
                cancelled,
                ..
            } => Some((*decision_id, *completed, *required, *cancelled)),
            _ => None,
        })
        .collect();
    assert_eq!(
        numbers,
        vec![
            (a, Some(1), Some(2), Some(0)),
            (b, Some(0), Some(1), Some(1))
        ]
    );
    let replay = w.replay();
    assert_eq!(replay, w.state);
    assert_eq!(replay.decision_redo(a), Some(earlier));
    assert_eq!(replay.decision_redo(b), Some(later));
}

// Break caught: recipients are computed after assignees were removed, or dedupe uses delivery.
#[test]
fn redo_notices_use_frozen_recipients_and_exact_sent_content() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let t = w.create(Some(d), None, None);
    w.assign(&t, "gamma/chief").unwrap();
    w.away();
    w.take(d);
    w.answer(d, "large").unwrap();
    let notes = w.state.redo_notices();
    assert_eq!(notes.len(), 2);
    assert_eq!(
        notes.iter().map(|n| n.to.to_string()).collect::<Vec<_>>(),
        ["owner", "gamma/chief"]
    );
    for note in notes {
        assert_eq!(note.body, format!("Task {t} needs redo after decision {d} was overruled: large\nIt was reopened and unassigned. Read the new answer before assigning or submitting it again. No files were reverted."));
        assert_eq!(note.task, Some(t.clone()));
        w.event(OrgEvent::MessageSent(note));
    }
    assert!(w.state.redo_notices().is_empty());
    assert_eq!(w.replay(), w.state);
}

// Break caught: redo changes are ignored by event equality or resolved events acquire resets.
#[test]
fn same_option_confirms_other_option_overrules_owner_only_and_metadata_is_semantic() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    w.away();
    w.take(d);
    let answer = DecisionResolution {
        decision_id: d,
        option: "large".into(),
        by: "gamma/chief".parse().unwrap(),
        note: None,
        standing_rule: false,
    };
    assert!(w
        .step(OrgCommand::Decision(DecisionCommand::Resolve(answer)))
        .is_err());
    assert!(
        matches!(w.answer(d, "small").unwrap().first(), Some(OrgEvent::DecisionResolved(e)) if e.redo_tasks.is_none())
    );
    let mut a: DecisionResolutionEvent = w
        .state
        .decision(&d)
        .unwrap()
        .resolution
        .clone()
        .unwrap()
        .into();
    let mut b = a.clone();
    b.caller_pane = Some("%9".into());
    assert_eq!(a, b);
    a.redo_tasks = Some(vec![]);
    assert_ne!(a, b);
    assert_eq!(
        serde_json::to_value(&a).unwrap()["redo_tasks"],
        serde_json::json!([])
    );
    assert!(serde_json::to_value(&b)
        .unwrap()
        .get("redo_tasks")
        .is_none());
}

// Break caught: invalidation drops pre-reset state time or erases cumulative baselines.
#[test]
fn overrule_preserves_cumulative_usage_and_active_state_time() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let t = w.create(Some(d), None, None);
    w.assign(&t, "beta/lead").unwrap();
    for (seconds, price) in [(0, 100), (4, 150)] {
        w.now = support::time() + chrono::Duration::seconds(seconds);
        w.event(OrgEvent::TaskUsageObserved(TaskUsageObserved {
            session_id: "s1".into(),
            task_id: Some(t.clone()),
            turn: true,
            cumulative_cost: Some(atm_core::Money::from_microdollars(price)),
            context_percent: Some(40),
        }));
    }
    w.now = support::time() + chrono::Duration::seconds(10);
    w.away();
    w.take(d);
    w.answer(d, "large").unwrap();
    let usage = w.state.task_cost(&t, w.now).unwrap();
    assert_eq!(usage.turns, Some(2));
    assert_eq!(usage.cost, Some(atm_core::Money::from_microdollars(50)));
    assert_eq!(usage.seconds_in_state.get(&TaskState::Assigned), Some(&10));
    assert_eq!(w.replay(), w.state);
}

// Break caught: reset assertions are vacuous because all attempt-local fields were empty.
#[test]
fn populated_review_and_accepted_work_clear_attempt_fields_but_keep_evidence() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let accepted = w.create(Some(d), None, None);
    w.assign(&accepted, "beta/lead").unwrap();
    change(&mut w, &accepted, TaskState::Accepted);
    w.event(OrgEvent::TaskCostRecorded(TaskCostRecorded {
        task_id: accepted.clone(),
        record: TaskCost {
            turns: Some(2),
            cost: Some(atm_core::Money::from_microdollars(7)),
            ..TaskCost::default()
        },
    }));
    let file = |path: &str, kind| TaskFile {
        path: path.into(),
        kind,
        note: Some("old attempt evidence".into()),
    };
    let seq = w.event(OrgEvent::TaskCreated(TaskCreated {
        project_id: None,
        title: "Read records".into(),
        kind: TaskKind::Work,
        creator: "beta/lead".parse().unwrap(),
        parent: Some(accepted.clone()),
        judges: None,
        measure: TaskMeasure::Reviewed {
            rubric: "rubric.md".into(),
        },
        budget: Some(TaskBudget {
            cost: Some(atm_core::Money::from_microdollars(20)),
            turns: Some(3),
            deadline: Some(w.now + chrono::Duration::hours(1)),
        }),
        files: vec![file("brief.md", TaskFileKind::Brief)],
        rests_on: Some(d),
    }));
    let in_review: TaskId = format!("t{seq}").parse().unwrap();
    let reviewer = w.create(None, None, Some(in_review.clone()));
    w.assign(&in_review, "beta/lead").unwrap();
    w.assign(&reviewer, "gamma/chief").unwrap();
    change(&mut w, &reviewer, TaskState::Working);
    w.event(OrgEvent::TaskSubmitted(TaskSubmitted {
        task_id: in_review.clone(),
        what: TaskSubmission::Result,
        files: vec![file("report.md", TaskFileKind::Report)],
        by: "beta/lead".parse().unwrap(),
    }));
    w.event(OrgEvent::TaskReviewStarted(TaskReviewStarted {
        task_id: in_review.clone(),
        review_task: reviewer.clone(),
        by: Address::Owner,
    }));
    // Synthetic late historical reports make cleanup of both markers non-vacuous.
    w.event(OrgEvent::TaskReminded(TaskReminded {
        task_id: in_review.clone(),
        agent_id: "beta/lead".parse().unwrap(),
    }));
    w.event(OrgEvent::TaskFlagged(TaskFlagged {
        task_id: in_review.clone(),
        to: Address::Owner,
    }));
    let active = w.state.task(&in_review).unwrap();
    assert_eq!(active.state, TaskState::InReview);
    assert_eq!(active.submission, Some(TaskSubmission::Result));
    assert_eq!(active.review_task, Some(reviewer.clone()));
    assert!(active.reminded_at.is_some() && active.flagged_at.is_some());
    assert_eq!(active.files.len(), 2);
    assert!(w.state.task(&accepted).unwrap().cost.is_some());
    let before: Vec<_> = [&accepted, &in_review, &reviewer]
        .into_iter()
        .map(|id| w.state.task(id).unwrap().clone())
        .collect();
    w.now += chrono::Duration::seconds(1);
    w.away();
    w.take(d);
    assert_eq!(w.answer(d, "large").unwrap().len(), 1);
    for mut expected in before {
        expected.state = TaskState::Open;
        expected.agent_id = None;
        expected.submission = None;
        expected.review_task = None;
        expected.reminded_at = None;
        expected.flagged_at = None;
        expected.cost = None;
        expected.reason = Some(format!("Decision {d} overruled; redo against large."));
        expected.changed_at = w.now;
        assert_eq!(
            w.state.task(&expected.id).unwrap(),
            &expected,
            "all other task fields, including links/budget/files, must be retained"
        );
        assert!(w.state.task_was_assigned(&expected.id));
    }
    let notes: Vec<_> = w
        .state
        .redo_notices()
        .into_iter()
        .filter(|note| note.task.as_ref() == Some(&in_review))
        .collect();
    assert_eq!(notes.len(), 1, "creator=former assignee gets one note");
    assert_eq!(notes.first().unwrap().to.to_string(), "beta/lead");
    assert_eq!(w.replay(), w.state);
}

// Break caught: the refusal omits a blocking task, or blames unrelated work.
#[test]
fn unknown_state_refusal_names_all_blocking_tasks() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let root = w.create(Some(d), None, None);
    let child = w.create(None, Some(root.clone()), None);
    let unrelated = w.create(None, None, None);
    for task in [&root, &child, &unrelated] {
        change(&mut w, task, TaskState::Unknown);
    }
    w.away();
    w.take(d);
    let before = w.state.clone();
    let refusal = w.answer(d, "large").unwrap_err();
    assert_eq!(w.state, before);
    let Refusal::InvalidValue { field, reason } = refusal else {
        panic!("unexpected refusal: {refusal:?}");
    };
    assert_eq!(field, "decision");
    for task in [&root, &child] {
        assert!(
            reason.contains(&task.to_string()),
            "missing {task}: {reason}"
        );
    }
    assert!(
        !reason.contains(&unrelated.to_string()),
        "unrelated task: {reason}"
    );
}

// Break caught: unknown live work is silently rewritten as if it were understood.
#[test]
fn unknown_live_dependency_refuses_the_overrule() {
    let mut w = World::new();
    let d = w.request("alpha/lead", false);
    let t = w.create(Some(d), None, None);
    change(&mut w, &t, TaskState::Unknown);
    w.away();
    w.take(d);
    let before = w.state.clone();
    assert!(w.answer(d, "large").is_err());
    assert_eq!(w.state, before);
}
