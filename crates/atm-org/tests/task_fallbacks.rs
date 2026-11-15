//! Synthetic future task values: read/fold them, never write their fallbacks.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_org::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tasks::*;
use TaskState as S;

fn future<T: DeserializeOwned>() -> T {
    serde_json::from_value(json!("future_value")).unwrap()
}
fn body() -> Value {
    serde_json::to_value(new_task(TaskKind::Work, mechanical(), LEAD)).unwrap()
}
fn future_file() -> TaskFile {
    serde_json::from_value(json!({"path":"future.dat","kind":"screenshot","note":"kept"})).unwrap()
}
fn future_cost() -> TaskCost {
    serde_json::from_value(json!({
        "turns":null,"review_rounds":null,"changes_requested":null,"cost":null,
        "peak_context_percent":null,"seconds_in_state":{"paused":7,"review_wait":11,"working":5}
    }))
    .unwrap()
}
fn fact(rig: &mut Rig, kind: &str, body: Value) -> OrgEvent {
    let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
    assert_eq!(
        OrgEvent::from_parts(kind, &event.to_body()).unwrap(),
        Some(event.clone()),
        "decoded event must have stable re-encoding"
    );
    rig.seq += 1;
    rig.state.apply(rig.seq, ts(rig.seq), &event).unwrap();
    rig.log.push((rig.seq, event.clone()));
    event
}
fn created(rig: &mut Rig, body: Value) -> TaskId {
    fact(rig, "task.created", body);
    tid(rig.seq)
}
fn submitted() -> (Rig, TaskId) {
    let mut rig = Rig::new();
    let task = rig.work_task();
    rig.ok(assign(&task, BUILD, by(LEAD)));
    rig.ok(submit(
        &task,
        TaskSubmission::Result,
        "result.md",
        by(BUILD),
    ));
    (rig, task)
}
fn invalid(rig: &mut Rig, command: TaskCommand, field: &'static str) {
    let before = (rig.state.clone(), rig.log.clone(), rig.seq);
    assert_eq!(
        rig.run(command),
        Err(Refusal::InvalidValue {
            field,
            reason: "must be known".into()
        })
    );
    assert_eq!((rig.state.clone(), rig.log.clone(), rig.seq), before);
}
fn frozen(rig: &mut Rig, task: &TaskId, command: TaskCommand) {
    let before = (rig.state.clone(), rig.log.clone(), rig.seq);
    match rig.run(command) {
        Err(Refusal::Task(TaskRefusal::InvalidState { task_id, state, .. })) => {
            assert_eq!(&task_id, task);
            assert_eq!(serde_json::to_value(state).unwrap(), json!("unknown"));
        }
        other => panic!("expected frozen task refusal, got {other:?}"),
    }
    assert_eq!((rig.state.clone(), rig.log.clone(), rig.seq), before);
}
fn set_future_state(rig: &mut Rig, task: &TaskId) {
    fact(
        rig,
        "task.state_changed",
        json!({
            "task_id":task,"state":"paused","reason":"future state","by":"owner"
        }),
    );
}
fn set_future_submission(rig: &mut Rig, task: &TaskId) {
    fact(
        rig,
        "task.submitted",
        json!({
            "task_id":task,"what":"draft","files":[],"by":BUILD
        }),
    );
}

#[test]
fn a_future_task_kind_folds_and_known_generic_moves_remain_available() {
    let mut rig = Rig::new();
    let mut raw = body();
    raw["kind"] = json!("spike");
    let task = created(&mut rig, raw.clone());
    assert_eq!(
        serde_json::to_value(rig.task(&task).kind).unwrap(),
        json!("unknown")
    );
    assert_eq!(rig.task(&task).state, S::Open);
    assert_eq!(raw["kind"], json!("spike"));
    rig.ok(assign(&task, BUILD, by(LEAD)));
    assert_eq!(rig.task(&task).state, S::Assigned);
    let judged = rig.work_task();
    rig.ok(assign(&judged, OTHER, by(LEAD)));
    rig.ok(submit(
        &judged,
        TaskSubmission::Result,
        "other.md",
        by(OTHER),
    ));
    assert!(matches!(
        rig.run(start_review(&judged, &task, by(LEAD))),
        Err(Refusal::Task(TaskRefusal::NotReviewOf { .. }))
    ));
}

#[test]
fn a_future_measure_folds_without_inventing_a_measure() {
    let mut rig = Rig::new();
    let mut raw = body();
    raw["measure"] = json!({"kind":"benchmark","metric":{"limit":42}});
    let task = created(&mut rig, raw.clone());
    assert_eq!(
        serde_json::to_value(&rig.task(&task).measure).unwrap(),
        json!({"kind":"unknown"})
    );
    assert_eq!(raw["measure"]["metric"]["limit"], json!(42));
    assert_eq!(rig.task(&task).state, S::Open);
}

#[test]
fn a_future_file_kind_keeps_the_file_and_does_not_freeze_the_task() {
    let mut rig = Rig::new();
    let mut raw = body();
    raw["files"] = json!([{"path":"future.dat","kind":"screenshot","note":"kept"}]);
    let task = created(&mut rig, raw);
    assert_eq!(
        serde_json::to_value(&rig.task(&task).files).unwrap(),
        json!([{"path":"future.dat","kind":"unknown","note":"kept"}])
    );
    rig.ok(TaskCommand::Attach {
        task_id: task.clone(),
        file: file("known.md", TaskFileKind::Report),
        by: by(LEAD),
    });
    let count = rig.task(&task).files.len();
    fact(
        &mut rig,
        "task.attached",
        json!({"task_id":task,
        "file":{"path":"future.dat","kind":"drawing","note":"kept"},"by":LEAD}),
    );
    assert_eq!(
        rig.task(&task).files.len(),
        count,
        "unknown kind words can coalesce in memory"
    );
}

#[test]
fn a_future_state_folds_with_its_attribution_and_later_known_facts_still_fold() {
    let (mut rig, task) = submitted();
    set_future_state(&mut rig, &task);
    assert_eq!(
        serde_json::to_value(rig.task(&task).state).unwrap(),
        json!("unknown")
    );
    assert_eq!(rig.task(&task).agent_id, Some(agent(BUILD)));
    assert_eq!(rig.task(&task).reason.as_deref(), Some("future state"));
    assert_eq!(rig.task(&task).changed_at, ts(rig.seq));
    fact(
        &mut rig,
        "task.state_changed",
        json!({"task_id":task,"state":"working","reason":null,"by":"owner"}),
    );
    assert_eq!(rig.task(&task).state, S::Working);
    rig.ok(submit(&task, TaskSubmission::Result, "known.md", by(BUILD)));
}

#[test]
fn a_future_verdict_folds_to_unknown_never_accepted() {
    let (mut rig, task) = submitted();
    let event = fact(
        &mut rig,
        "task.verdict",
        json!({"task_id":task,
        "verdict":"deferred","review_task":null,"note":"later","by":"owner"}),
    );
    assert_eq!(event.to_body()["verdict"], json!("unknown"));
    assert_eq!(
        serde_json::to_value(rig.task(&task).state).unwrap(),
        json!("unknown")
    );
    assert_eq!(rig.task(&task).reason.as_deref(), Some("later"));
    assert_eq!(rig.task(&task).submission, None);
}

#[test]
fn a_future_submission_is_not_assumed_to_be_a_result_on_acceptance() {
    for (given, expected) in [
        ("accepted", "unknown"),
        ("changes_requested", "changes_requested"),
    ] {
        let (mut rig, task) = submitted();
        let event = fact(
            &mut rig,
            "task.submitted",
            json!({"task_id":task,"what":"draft","files":[],"by":BUILD}),
        );
        assert_eq!(event.to_body()["what"], json!("unknown"));
        assert_eq!(rig.task(&task).state, S::Submitted);
        assert_eq!(
            serde_json::to_value(rig.task(&task).submission).unwrap(),
            json!("unknown")
        );
        fact(
            &mut rig,
            "task.verdict",
            json!({"task_id":task,"verdict":given,"review_task":null,"note":null,"by":"owner"}),
        );
        assert_eq!(
            serde_json::to_value(rig.task(&task).state).unwrap(),
            json!(expected)
        );
    }
}

#[test]
fn unknown_seconds_keys_are_summed_with_saturation_and_known_buckets_unchanged() {
    for (seconds, expected) in [
        (json!({"paused":7,"review_wait":11,"working":5}), 18),
        (
            json!({"paused":u64::MAX,"review_wait":1,"working":5}),
            u64::MAX,
        ),
        (json!({"unknown":2,"future":3,"working":5}), 5),
    ] {
        let (mut rig, task) = submitted();
        let event = fact(
            &mut rig,
            "task.cost_recorded",
            json!({"task_id":task,"record":{
            "turns":null,"review_rounds":null,"changes_requested":null,"cost":null,
            "peak_context_percent":null,"seconds_in_state":seconds}}),
        );
        let want = json!({"unknown":expected,"working":5});
        assert_eq!(event.to_body()["record"]["seconds_in_state"], want);
        assert_eq!(
            serde_json::to_value(&rig.task(&task).cost.as_ref().unwrap().seconds_in_state).unwrap(),
            want
        );
    }
    let mut de = serde_json::Deserializer::from_str(
        r#"{"turns":null,"review_rounds":null,"changes_requested":null,"cost":null,"peak_context_percent":null,"seconds_in_state":{"working":1,"working":9}}"#,
    );
    let cost = <TaskCost as serde::Deserialize>::deserialize(&mut de).unwrap();
    assert_eq!(
        cost.seconds_in_state.get(&S::Working),
        Some(&9),
        "known duplicate keys retain their existing last-value behavior"
    );
}

#[test]
fn commands_carrying_each_fallback_are_refused() {
    let (mut rig, task) = submitted();
    let mut kind = body();
    kind["kind"] = json!("spike");
    let mut measure = body();
    measure["measure"] = json!({"kind":"benchmark","metric":1});
    let mut files = body();
    files["files"] = serde_json::to_value(vec![future_file()]).unwrap();
    for (command, field) in [
        (
            TaskCommand::Create(serde_json::from_value(kind).unwrap()),
            "kind",
        ),
        (
            TaskCommand::Create(serde_json::from_value(measure).unwrap()),
            "measure.kind",
        ),
        (
            TaskCommand::Create(serde_json::from_value(files).unwrap()),
            "file.kind",
        ),
        (set(&task, future(), None, by(BUILD)), "state"),
        (
            TaskCommand::Submit {
                task_id: task.clone(),
                what: future(),
                files: vec![],
                by: by(BUILD),
            },
            "what",
        ),
        (
            TaskCommand::Submit {
                task_id: task.clone(),
                what: TaskSubmission::Result,
                files: vec![future_file()],
                by: by(BUILD),
            },
            "file.kind",
        ),
        (
            TaskCommand::Attach {
                task_id: task.clone(),
                file: future_file(),
                by: by("owner"),
            },
            "file.kind",
        ),
        (verdict(&task, future(), None, by("owner")), "verdict"),
        (
            TaskCommand::RecordCost {
                task_id: task.clone(),
                record: future_cost(),
            },
            "seconds_in_state",
        ),
    ] {
        invalid(&mut rig, command, field);
    }
}

#[test]
fn fallback_refusals_precede_duplicate_and_repeat_shortcuts() {
    let (mut rig, task) = submitted();
    fact(
        &mut rig,
        "task.attached",
        json!({"task_id":task,"file":serde_json::to_value(future_file()).unwrap(),"by":LEAD}),
    );
    invalid(
        &mut rig,
        TaskCommand::Attach {
            task_id: task.clone(),
            file: future_file(),
            by: by(LEAD),
        },
        "file.kind",
    );
    invalid(
        &mut rig,
        TaskCommand::Submit {
            task_id: task.clone(),
            what: TaskSubmission::Result,
            files: vec![future_file()],
            by: by(BUILD),
        },
        "file.kind",
    );
    set_future_submission(&mut rig, &task);
    invalid(
        &mut rig,
        TaskCommand::Submit {
            task_id: task.clone(),
            what: future(),
            files: vec![],
            by: by(BUILD),
        },
        "what",
    );
    fact(
        &mut rig,
        "task.verdict",
        json!({"task_id":task,"verdict":"accepted","review_task":null,"note":null,"by":"owner"}),
    );
    fact(
        &mut rig,
        "task.state_changed",
        json!({"task_id":task,"state":"accepted","reason":null,"by":"owner"}),
    );
    let cost = future_cost();
    fact(
        &mut rig,
        "task.cost_recorded",
        json!({"task_id":task,"record":serde_json::to_value(&cost).unwrap()}),
    );
    invalid(
        &mut rig,
        TaskCommand::RecordCost {
            task_id: task,
            record: cost,
        },
        "seconds_in_state",
    );
}

#[test]
fn unknown_state_freezes_every_command_and_every_known_transition() {
    let (mut rig, task) = submitted();
    let review = rig.review_task(&task);
    set_future_state(&mut rig, &task);
    for who in [
        by(LEAD),
        as_lead(BOSS),
        by("owner"),
        by(BUILD),
        by(OTHER),
        by("system"),
    ] {
        let mut commands = vec![
            assign(&task, OTHER, who.clone()),
            submit(&task, TaskSubmission::Plan, "plan.md", who.clone()),
            submit(&task, TaskSubmission::Result, "result.md", who.clone()),
            start_review(&task, &review, who.clone()),
            verdict(&task, TaskVerdict::Accepted, None, who.clone()),
            verdict(&task, TaskVerdict::ChangesRequested, None, who.clone()),
            TaskCommand::Attach {
                task_id: task.clone(),
                file: file("brief.md", TaskFileKind::Brief),
                by: who,
            },
            TaskCommand::Reminded {
                task_id: task.clone(),
                agent_id: agent(BUILD),
            },
            TaskCommand::Flagged {
                task_id: task.clone(),
                to: LEAD.parse().unwrap(),
            },
            TaskCommand::RecordCost {
                task_id: task.clone(),
                record: TaskCost::default(),
            },
        ];
        commands.extend(
            [
                S::Open,
                S::Assigned,
                S::Working,
                S::Waiting,
                S::Blocked,
                S::Submitted,
                S::InReview,
                S::Accepted,
                S::ChangesRequested,
                S::Cancelled,
            ]
            .map(|state| set(&task, state, Some("why"), by("owner"))),
        );
        for command in commands {
            frozen(&mut rig, &task, command);
        }
    }
    let mut child = new_task(TaskKind::Work, mechanical(), LEAD);
    child.parent = Some(task.clone());
    rig.create(child);
    rig.review_task(&task);
}

#[test]
fn an_unknown_state_review_is_refused_even_on_repeated_start() {
    for started in [false, true] {
        let (mut rig, task) = submitted();
        let review = rig.submitted_review(&task);
        if started {
            rig.ok(start_review(&task, &review, by(LEAD)));
        }
        set_future_state(&mut rig, &review);
        for who in [by(LEAD), as_lead(BOSS), by("owner")] {
            frozen(&mut rig, &review, start_review(&task, &review, who.clone()));
            for given in [TaskVerdict::Accepted, TaskVerdict::ChangesRequested] {
                frozen(
                    &mut rig,
                    &review,
                    verdict(&task, given, Some(&review), who.clone()),
                );
            }
        }
    }
}

#[test]
fn an_unknown_submission_blocks_review_and_verdict_but_can_be_withdrawn() {
    for started in [false, true] {
        let (mut rig, task) = submitted();
        let review = rig.submitted_review(&task);
        set_future_submission(&mut rig, &task);
        if started {
            fact(
                &mut rig,
                "task.review_started",
                json!({"task_id":task,"review_task":review,"by":LEAD}),
            );
        }
        for who in [by(LEAD), as_lead(BOSS), by("owner")] {
            invalid(
                &mut rig,
                start_review(&task, &review, who.clone()),
                "submission",
            );
            for given in [TaskVerdict::Accepted, TaskVerdict::ChangesRequested] {
                invalid(
                    &mut rig,
                    verdict(&task, given, Some(&review), who.clone()),
                    "submission",
                );
            }
        }
        if started {
            rig.ok(set(&task, S::Cancelled, None, by("owner")));
        } else {
            rig.ok(set(&task, S::Working, None, by(BUILD)));
        }
    }
    let (mut rig, task) = submitted();
    let review = rig.submitted_review(&task);
    set_future_submission(&mut rig, &review);
    assert!(matches!(
        rig.run(verdict(
            &task,
            TaskVerdict::Accepted,
            Some(&review),
            by(LEAD)
        )),
        Err(Refusal::Task(TaskRefusal::ReviewNotSubmitted { .. }))
    ));
}

#[test]
fn unknown_measure_blocks_direct_results_not_plans_changes_or_review_bookkeeping() {
    let mut rig = Rig::new();
    let mut raw = body();
    raw["measure"] = json!({"kind":"benchmark","metric":1});
    let task = created(&mut rig, raw.clone());
    rig.ok(assign(&task, BUILD, by(LEAD)));
    rig.ok(submit(
        &task,
        TaskSubmission::Result,
        "result.md",
        by(BUILD),
    ));
    for who in [by(LEAD), as_lead(BOSS), by("owner")] {
        let before = rig.state.clone();
        assert_eq!(
            rig.run(verdict(&task, TaskVerdict::Accepted, None, who)),
            Err(Refusal::Task(TaskRefusal::Measure {
                task_id: task.clone(),
                needs: "a known measure"
            }))
        );
        assert_eq!(rig.state, before);
    }
    rig.ok(verdict(
        &task,
        TaskVerdict::ChangesRequested,
        None,
        by(LEAD),
    ));
    rig.ok(submit(&task, TaskSubmission::Plan, "plan.md", by(BUILD)));
    rig.ok(verdict(&task, TaskVerdict::Accepted, None, by(LEAD)));
    assert_eq!(rig.task(&task).state, S::Working);

    let judged = rig.work_task();
    rig.ok(assign(&judged, BUILD, by(LEAD)));
    rig.ok(submit(
        &judged,
        TaskSubmission::Result,
        "judged.md",
        by(BUILD),
    ));
    raw["kind"] = json!("review");
    raw["judges"] = json!(judged);
    let review = created(&mut rig, raw);
    rig.ok(assign(&review, REVIEWER, by(LEAD)));
    rig.ok(submit(
        &review,
        TaskSubmission::Result,
        "review.md",
        by(REVIEWER),
    ));
    assert_eq!(
        rig.ok(verdict(
            &judged,
            TaskVerdict::Accepted,
            Some(&review),
            by(LEAD)
        ))
        .len(),
        2
    );
    assert_eq!(rig.task(&review).state, S::Accepted);
}

#[test]
fn malformed_types_known_variants_ids_and_required_fields_remain_errors() {
    for (field, bad) in [
        ("kind", json!(99)),
        ("measure", json!({"kind":99})),
        ("measure", json!({"kind":"mechanical","command":99})),
        ("measure", json!({"kind":"reviewed"})),
        ("files", json!([{"path":"x","kind":99,"note":null}])),
        ("rests_on", json!("7")),
    ] {
        let mut raw = body();
        raw[field] = bad;
        assert!(
            OrgEvent::from_parts("task.created", &raw).is_err(),
            "{field}: {raw}"
        );
    }
    for (kind, raw) in [
        (
            "task.state_changed",
            json!({"task_id":"t7","state":99,"reason":null,"by":"owner"}),
        ),
        (
            "task.verdict",
            json!({"task_id":"t7","verdict":99,"review_task":null,"note":null,"by":"owner"}),
        ),
        (
            "task.submitted",
            json!({"task_id":"t7","what":99,"files":[],"by":"owner"}),
        ),
        (
            "task.cost_recorded",
            json!({"task_id":"t7","record":{"seconds_in_state":{"future":"bad"}}}),
        ),
        ("task.cost_recorded", json!({"task_id":"t7","record":{}})),
    ] {
        assert!(OrgEvent::from_parts(kind, &raw).is_err(), "{kind}: {raw}");
    }
    let mut rig = Rig::new();
    let before = rig.state.clone();
    let event = OrgEvent::from_parts(
        "task.state_changed",
        &json!({"task_id":"t999999","state":"paused","reason":null,"by":"owner"}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        rig.state.apply(99, ts(99), &event),
        Err(ApplyError::UnknownTask(tid(999999)))
    );
    assert_eq!(rig.state, before);
}
