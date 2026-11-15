//! Synthetic future-value histories and the no-fallback-writer invariant.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;
#[path = "support/tasks.rs"]
mod tasks;
use atm_org::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde_json::{json, Value};
use tasks::*;

fn created() -> Value {
    serde_json::to_value(new_task(TaskKind::Work, mechanical(), "owner")).unwrap()
}
fn log(word: &str, a: u64, b: u64) -> Vec<(u64, String, Value)> {
    let mut kind = created();
    kind["kind"] = json!(word);
    let mut measure = created();
    measure["measure"] = json!({"kind":word,"future_payload":[1,null,true]});
    let mut files = created();
    files["files"] = json!([{"path":"future.dat","kind":word,"note":null}]);
    [
        ("task.created", kind),
        ("task.created", measure),
        ("task.created", files),
        ("task.state_changed", json!({"task_id":"t1","state":word,"reason":null,"by":"owner"})),
        ("task.submitted", json!({"task_id":"t2","what":word,"files":[],"by":"owner"})),
        ("task.verdict", json!({"task_id":"t3","verdict":word,"review_task":null,"note":null,"by":"owner"})),
        ("task.cost_recorded", json!({"task_id":"t3","record":{
            "seconds_in_state":{(format!("a_{word}")):a,(format!("b_{word}")):b,"working":3}}})),
        ("task.state_changed", json!({"task_id":"t1","state":"working","reason":null,"by":"owner"})),
        ("task.verdict", json!({"task_id":"t2","verdict":"accepted","review_task":null,"note":null,"by":"owner"})),
    ].into_iter().enumerate().map(|(index, (kind, body))| (index as u64 + 1, kind.into(), body)).collect()
}
fn fold(log: &[(u64, String, Value)], state: &mut OrgState, stats: &mut ReplayStats) {
    for (seq, kind, body) in log {
        stats
            .apply_parts(state, *seq, ts(*seq), kind, body)
            .unwrap();
    }
}
fn known_files(files: &[TaskFile]) -> bool {
    files.iter().all(|file| file.kind != TaskFileKind::Unknown)
}
fn assert_known_payload(event: &OrgEvent) {
    let known = match event {
        OrgEvent::TaskCreated(body) => {
            body.kind != TaskKind::Unknown
                && body.measure != TaskMeasure::Unknown
                && known_files(&body.files)
        }
        OrgEvent::TaskStateChanged(body) => body.state != TaskState::Unknown,
        OrgEvent::TaskSubmitted(body) => {
            body.what != TaskSubmission::Unknown && known_files(&body.files)
        }
        OrgEvent::TaskVerdict(body) => body.verdict != TaskVerdict::Unknown,
        OrgEvent::TaskAttached(body) => body.file.kind != TaskFileKind::Unknown,
        OrgEvent::TaskCostRecorded(body) => !body
            .record
            .seconds_in_state
            .contains_key(&TaskState::Unknown),
        OrgEvent::TaskAssigned(_)
        | OrgEvent::TaskReviewStarted(_)
        | OrgEvent::TaskReminded(_)
        | OrgEvent::TaskFlagged(_) => true,
        other => panic!("task command returned a non-task event: {other:?}"),
    };
    assert!(known, "policy emitted a read-only fallback: {event:?}");
}

#[test]
fn future_value_histories_fold_deterministically_and_compose_at_every_prefix() {
    let mut runner = TestRunner::new(Config {
        cases: 128,
        rng_seed: RngSeed::Fixed(0x71bf),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(
            &("[a-z]{0,24}", any::<u64>(), any::<u64>()),
            |(suffix, a, b)| {
                let word = format!("future_{suffix}");
                let log = log(&word, a, b);
                let mut whole = OrgState::default();
                let mut stats = ReplayStats::default();
                fold(&log, &mut whole, &mut stats);
                prop_assert_eq!(stats.applied, 9);
                prop_assert_eq!(stats.skipped_unknown, 0);
                prop_assert_eq!(whole.task(&tid(1)).unwrap().kind, TaskKind::Unknown);
                prop_assert_eq!(whole.task(&tid(1)).unwrap().state, TaskState::Working);
                prop_assert_eq!(&whole.task(&tid(2)).unwrap().measure, &TaskMeasure::Unknown);
                prop_assert_eq!(whole.task(&tid(2)).unwrap().state, TaskState::Unknown);
                let task = whole.task(&tid(3)).unwrap();
                prop_assert_eq!(task.state, TaskState::Unknown);
                prop_assert_eq!(task.files.first().unwrap().kind, TaskFileKind::Unknown);
                let seconds = &task.cost.as_ref().unwrap().seconds_in_state;
                prop_assert_eq!(seconds.get(&TaskState::Unknown), Some(&a.saturating_add(b)));
                prop_assert_eq!(seconds.get(&TaskState::Working), Some(&3));
                for cut in 0..=log.len() {
                    let mut partial = OrgState::default();
                    let mut partial_stats = ReplayStats::default();
                    fold(&log[..cut], &mut partial, &mut partial_stats);
                    fold(&log[cut..], &mut partial, &mut partial_stats);
                    prop_assert_eq!(&partial, &whole);
                    prop_assert_eq!(&partial_stats, &stats);
                }
                for (_, kind, body) in &log {
                    let event = OrgEvent::from_parts(kind, body).unwrap().unwrap();
                    prop_assert_eq!(
                        OrgEvent::from_parts(kind, &event.to_body()).unwrap(),
                        Some(event)
                    );
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn decided_task_payloads_never_contain_fallback_values() {
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x71b0),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(
            &(0usize..9, any::<bool>(), "[a-z]{0,24}", any::<u64>()),
            |(op, unknown, suffix, seconds)| {
                let word = format!("future_{suffix}");
                let mut rig = Rig::new();
                let task = rig.work_task();
                rig.ok(assign(&task, BUILD, by(LEAD)));
                if op == 6 || op == 8 {
                    rig.ok(submit(
                        &task,
                        TaskSubmission::Result,
                        "result.md",
                        by(BUILD),
                    ));
                }
                if op == 8 {
                    rig.ok(verdict(&task, TaskVerdict::Accepted, None, by(LEAD)));
                }
                let file: TaskFile = serde_json::from_value(json!({"path":"run.dat",
            "kind":if unknown { word.as_str() } else { "evidence" },"note":null}))
                .unwrap();
                let (command, field) = match op {
                    0..=2 => {
                        let mut body =
                            serde_json::to_value(new_task(TaskKind::Work, mechanical(), LEAD))
                                .unwrap();
                        let field = match op {
                            0 => {
                                body["kind"] = json!(if unknown { word.as_str() } else { "work" });
                                "kind"
                            }
                            1 => {
                                body["measure"] = if unknown {
                                    json!({"kind":word})
                                } else {
                                    json!({"kind":"owner"})
                                };
                                "measure.kind"
                            }
                            _ => {
                                body["files"] = json!([file]);
                                "file.kind"
                            }
                        };
                        (
                            TaskCommand::Create(serde_json::from_value(body).unwrap()),
                            field,
                        )
                    }
                    3 => (
                        set(
                            &task,
                            serde_json::from_value(json!(if unknown {
                                word.as_str()
                            } else {
                                "working"
                            }))
                            .unwrap(),
                            None,
                            by(BUILD),
                        ),
                        "state",
                    ),
                    4 => (
                        TaskCommand::Submit {
                            task_id: task.clone(),
                            what: serde_json::from_value(json!(if unknown {
                                word.as_str()
                            } else {
                                "result"
                            }))
                            .unwrap(),
                            files: vec![],
                            by: by(BUILD),
                        },
                        "what",
                    ),
                    5 => (
                        TaskCommand::Submit {
                            task_id: task.clone(),
                            what: TaskSubmission::Result,
                            files: vec![file],
                            by: by(BUILD),
                        },
                        "file.kind",
                    ),
                    6 => (
                        verdict(
                            &task,
                            serde_json::from_value(json!(if unknown {
                                word.as_str()
                            } else {
                                "accepted"
                            }))
                            .unwrap(),
                            None,
                            by(LEAD),
                        ),
                        "verdict",
                    ),
                    7 => (
                        TaskCommand::Attach {
                            task_id: task.clone(),
                            file,
                            by: by(LEAD),
                        },
                        "file.kind",
                    ),
                    _ => {
                        let key = if unknown { word.as_str() } else { "working" };
                        let record =
                            serde_json::from_value(json!({"seconds_in_state":{(key):seconds}}))
                                .unwrap();
                        (
                            TaskCommand::RecordCost {
                                task_id: task,
                                record,
                            },
                            "seconds_in_state",
                        )
                    }
                };
                let before = rig.state.clone();
                let result = rig.run(command);
                if unknown {
                    prop_assert_eq!(
                        result,
                        Err(Refusal::InvalidValue {
                            field,
                            reason: "must be known".into()
                        })
                    );
                    prop_assert_eq!(&rig.state, &before);
                } else {
                    for event in result.unwrap() {
                        assert_known_payload(&event);
                    }
                }
                Ok(())
            },
        )
        .unwrap();
}
