//! Worker ownership and partial-write contracts; gates are events, not delays.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::server::methods::{CallerId, CallerPane};
use atm_org::{TaskCreated, TaskMeasure, TaskVerdict, TaskVerdictGiven};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

fn caller() -> CallContext {
    CallContext {
        client_id: CallerId("worker-test".into()),
        from_pane: Some(CallerPane("%1".into())),
    }
}
fn created(title: String) -> TaskCreated {
    TaskCreated {
        title,
        kind: TaskKind::Work,
        creator: Address::Owner,
        parent: None,
        judges: None,
        measure: TaskMeasure::Owner,
        budget: None,
        files: vec![],
        rests_on: None,
    }
}
async fn org() -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgCommand::Task(TaskCommand::Create(created("Work".into()))),
    ] {
        org.execute(command, chrono::Utc::now()).await.unwrap();
    }
    (dir, org, cancel)
}
fn assign(_: TaskActor) -> TaskCommand {
    TaskCommand::Assign {
        task_id: "t3".parse().unwrap(),
        agent_id: "p/worker".parse().unwrap(),
        by: TaskActor {
            who: Address::Owner,
            lead: false,
        },
    }
}

fn owner_caller() -> CallContext {
    CallContext {
        from_pane: None,
        ..caller()
    }
}
fn verdict(by: TaskActor, verdict: TaskVerdict) -> TaskCommand {
    TaskCommand::Verdict {
        task_id: "t3".parse().unwrap(),
        verdict,
        review_task: None,
        note: None,
        by,
    }
}
async fn verdict_org() -> (tempfile::TempDir, OrgHandle, CancellationToken) {
    let rig = org().await;
    for command in [
        assign(TaskActor {
            who: Address::Owner,
            lead: false,
        }),
        TaskCommand::Submit {
            task_id: "t3".parse().unwrap(),
            what: TaskSubmission::Plan,
            files: vec![],
            by: TaskActor {
                who: "p/worker".parse().unwrap(),
                lead: false,
            },
        },
    ] {
        rig.1
            .execute(OrgCommand::Task(command), chrono::Utc::now())
            .await
            .unwrap();
    }
    rig
}
fn verdict_snapshot(wanted: TaskState, assigned: bool) -> OrgState {
    let mut state = OrgState::default();
    let now = chrono::Utc::now();
    for (seq, event) in [
        OrgEvent::ProjectRegistered {
            project_id: "p".parse().unwrap(),
            root: "/p".into(),
        },
        OrgEvent::AgentAdopted {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        OrgEvent::TaskCreated(created("Work".into())),
    ]
    .into_iter()
    .enumerate()
    {
        state.apply(seq as u64 + 1, now, &event).unwrap();
    }
    if assigned {
        state
            .apply(
                4,
                now,
                &OrgEvent::TaskAssigned(atm_org::TaskAssigned {
                    task_id: "t3".parse().unwrap(),
                    agent_id: "p/worker".parse().unwrap(),
                    by: Address::Owner,
                }),
            )
            .unwrap();
    }
    state
        .apply(
            5,
            now,
            &OrgEvent::TaskStateChanged(atm_org::TaskStateChanged {
                task_id: "t3".parse().unwrap(),
                state: wanted,
                reason: None,
                by: Address::Owner,
            }),
        )
        .unwrap();
    state
}
fn verdict_fact(verdict: TaskVerdict) -> TaskVerdictGiven {
    TaskVerdictGiven {
        task_id: "t3".parse().unwrap(),
        verdict,
        review_task: None,
        note: None,
        by: Address::Owner,
    }
}

// Break caught: a late actor receipt is discarded, losing the brief/report/answer/notice forever.
#[tokio::test(start_paused = true)]
async fn slow_actor_retains_assignment_brief() {
    late_actor_message(
        TaskState::Open,
        assign,
        owner_caller(),
        TaskState::Assigned,
        "p/worker",
        MessageKind::Brief,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn slow_actor_retains_submission_report() {
    late_actor_message(
        TaskState::Assigned,
        |by| TaskCommand::Submit {
            task_id: "t3".parse().unwrap(),
            what: TaskSubmission::Plan,
            files: vec![],
            by,
        },
        caller(),
        TaskState::Submitted,
        "owner",
        MessageKind::Report,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn slow_actor_retains_changes_requested_answer() {
    late_actor_message(
        TaskState::Submitted,
        |by| verdict(by, TaskVerdict::ChangesRequested),
        owner_caller(),
        TaskState::ChangesRequested,
        "p/worker",
        MessageKind::Answer,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn slow_actor_retains_accepted_plan_answer() {
    late_actor_message(
        TaskState::Submitted,
        |by| verdict(by, TaskVerdict::Accepted),
        owner_caller(),
        TaskState::Working,
        "p/worker",
        MessageKind::Answer,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn slow_actor_retains_invalidation_notice() {
    late_actor_message(
        TaskState::Assigned,
        |by| TaskCommand::SetState {
            task_id: "t3".parse().unwrap(),
            state: TaskState::Cancelled,
            reason: None,
            by,
        },
        owner_caller(),
        TaskState::Cancelled,
        "p/worker",
        MessageKind::Note,
    )
    .await;
}

async fn late_actor_message(
    before: TaskState,
    make: fn(TaskActor) -> TaskCommand,
    caller: CallContext,
    wanted: TaskState,
    to: &str,
    kind: MessageKind,
) {
    use crate::{eventlog::Envelope, org::Request};
    use tokio::sync::{broadcast, watch};

    let mut state = verdict_snapshot(before, before != TaskState::Open);
    if before == TaskState::Submitted {
        state
            .apply(
                6,
                chrono::Utc::now(),
                &OrgEvent::TaskSubmitted(atm_org::TaskSubmitted {
                    task_id: "t3".parse().unwrap(),
                    what: TaskSubmission::Plan,
                    files: vec![],
                    by: "p/worker".parse().unwrap(),
                }),
            )
            .unwrap();
    }
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(6));
    let (committed, _) = broadcast::channel(32);
    let cancel = CancellationToken::new();
    let (entered, mut entry) = mpsc::unbounded_channel();
    let mut seq = 6;
    let actor = tokio::spawn(crate::org::actor::run(
        state,
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        move |envelope: Envelope| {
            seq += 1;
            let seq = seq;
            let gate = if envelope.kind == "control.sent" {
                let (release, gate) = oneshot::channel();
                entered.send(release).unwrap();
                Some(gate)
            } else {
                None
            };
            async move {
                if let Some(gate) = gate {
                    // Match the production append bound, including queue admission.
                    tokio::time::timeout(Duration::from_secs(5), gate)
                        .await
                        .expect("predecessor append exceeded its own deadline")
                        .unwrap();
                }
                Ok(seq)
            }
        },
    ));
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: None,
    };
    let tasks = TaskHandle::new(org.clone());
    let workers = tasks.workers.clone();
    let forward = org.clone();
    let calling = tasks.clone();
    let context = caller.clone();
    let call = tokio::spawn(async move {
        calling
            .mutate(context, move |by| {
                // The identity snapshot has completed. Queue finite actor work
                // ahead of the task command, not ahead of its caller lookup.
                for _ in 0..2 {
                    let (reply, _) = oneshot::channel();
                    forward
                        .submit(Request::Execute {
                            command: Box::new(OrgCommand::ControlSent(atm_org::ControlAudit {
                                pane: "%1".into(),
                                action: serde_json::json!({"test": "actor backlog"}),
                            })),
                            now: chrono::Utc::now(),
                            caller_pane: None,
                            reply,
                        })
                        .unwrap();
                }
                make(by)
            })
            .await
    });
    let first = entry.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(4)).await;
    first.send(()).unwrap();
    let second = entry.recv().await.unwrap();
    let error = call.await.unwrap().unwrap_err();
    assert_eq!(MethodError::from(error).0.code, "timeout");
    let retained = workers.clone().try_acquire_many_owned(64).is_err();
    second.send(()).unwrap();
    drop(workers.acquire_many(64).await.unwrap());

    let state = org.snapshot().await.unwrap();
    assert_eq!(state.task(&"t3".parse().unwrap()).unwrap().state, wanted);
    let messages: Vec<_> = state.messages().map(|m| m.content.clone()).collect();
    assert_eq!(messages.len(), 1, "late commit lost its automatic message");
    assert!(retained, "RPC timeout released the task worker early");
    let message = messages.first().unwrap();
    assert_eq!(message.to.to_string(), to);
    assert_eq!(message.kind, kind);
    assert_eq!(message.task.as_ref().unwrap().to_string(), "t3");

    let retry = tasks.mutate(caller, make).await;
    if wanted == TaskState::Working {
        // An accepted plan is already Working; another verdict is refused.
        assert!(matches!(
            retry,
            Err(TaskError::Organisation(OrgServiceError::Refused(_)))
        ));
    } else {
        let retry = retry.unwrap();
        assert!(!retry.changed);
        assert!(retry.notification.is_none());
    }
    assert_eq!(
        org.snapshot()
            .await
            .unwrap()
            .messages()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>(),
        messages,
        "retry duplicated the automatic message"
    );
    cancel.cancel();
    actor.await.unwrap();
}

// Break caught: an identity snapshot, rather than the actor's actual before-image, routes revocation.
#[tokio::test]
async fn ownership_routing_uses_actor_beforeimage() {
    use crate::org::Request;
    let (_dir, real, cancel) = org().await;
    for (name, pane) in [("middle", "%2"), ("next", "%3")] {
        real.execute(
            OrgCommand::Adopt {
                agent_id: format!("p/{name}").parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }
    real.execute(
        OrgCommand::Task(assign(TaskActor {
            who: Address::Owner,
            lead: false,
        })),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let (requests, mut receive) = mpsc::channel(8);
    let proxy = OrgHandle {
        requests,
        ..real.clone()
    };
    let (entered, entry) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let forward = real.clone();
    let forwarding = tokio::spawn(async move {
        let mut pause = Some((entered, gate));
        while let Some(request) = receive.recv().await {
            if matches!(&request, Request::Execute { command, .. } if matches!(command.as_ref(), OrgCommand::Task(_)))
            {
                let (entered, gate) = pause.take().unwrap();
                entered.send(()).unwrap();
                gate.await.unwrap();
            }
            forward.submit(request).unwrap();
        }
    });
    let sender = real.clone();
    let call = tokio::spawn(execute_mutation(
        proxy,
        owner_caller(),
        |by| TaskCommand::Assign {
            task_id: "t3".parse().unwrap(),
            agent_id: "p/next".parse().unwrap(),
            by,
        },
        move |content| enqueue(sender.clone(), content),
    ));
    entry.await.unwrap(); // identity snapshot has been read, task mutation has not reached the actor.
    real.execute(
        OrgCommand::Task(TaskCommand::Assign {
            task_id: "t3".parse().unwrap(),
            agent_id: "p/middle".parse().unwrap(),
            by: TaskActor {
                who: Address::Owner,
                lead: false,
            },
        }),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    release.send(()).unwrap();
    let result = call.await.unwrap().unwrap();
    assert!(matches!(
        result.notification,
        Some(Notification::Queued { .. })
    ));
    let state = real.snapshot().await.unwrap();
    let audiences: Vec<_> = state
        .messages()
        .map(|m| (m.content.to.to_string(), m.content.kind))
        .collect();
    assert_eq!(
        audiences,
        [
            ("p/next".into(), MessageKind::Brief),
            ("p/middle".into(), MessageKind::Note)
        ],
        "missing actual previous-holder notice"
    );
    forwarding.await.unwrap();
    cancel.cancel();
}

// Break caught: partial fanout stops early/lies about queued truth, or waiter loss drops worker ownership.
#[tokio::test]
async fn partial_fanout_retains_worker_and_committed_truth() {
    for lost_waiter in [0, 1, 2] {
        // await, abort, timeout
        for failed in [0, 1] {
            let (_dir, org, cancel) = org().await;
            for (name, pane) in [("lead", "%2"), ("next", "%3")] {
                org.execute(
                    OrgCommand::Adopt {
                        agent_id: format!("p/{name}").parse().unwrap(),
                        pane: pane.into(),
                        session_id: None,
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            }
            org.execute(
                OrgCommand::Task(assign(TaskActor {
                    who: Address::Owner,
                    lead: false,
                })),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            let tasks = TaskHandle::new(org.clone());
            let workers = tasks.workers.clone();
            let sender = org.clone();
            let (entered, mut entry) = mpsc::unbounded_channel();
            let (done, finished) = oneshot::channel();
            let call = tokio::spawn(async move {
                let worker_org = tasks.org.clone();
                tasks
                    .run(async move {
                        let result = execute_mutation(
                            worker_org,
                            CallContext {
                                from_pane: Some(CallerPane("%2".into())),
                                ..caller()
                            },
                            |by| TaskCommand::Assign {
                                task_id: "t3".parse().unwrap(),
                                agent_id: "p/next".parse().unwrap(),
                                by,
                            },
                            move |content| {
                                let (release, gate) = oneshot::channel();
                                entered.send((content.clone(), release)).unwrap();
                                let sender = sender.clone();
                                async move {
                                    if gate.await.unwrap() {
                                        enqueue(sender, content).await
                                    } else {
                                        Err(TaskError::Organisation(OrgServiceError::Unavailable(
                                            "test send failed".into(),
                                        )))
                                    }
                                }
                            },
                        )
                        .await
                        .unwrap();
                        done.send(serde_json::to_value(result).unwrap()).unwrap();
                        Ok(())
                    })
                    .await
            });
            let (first, release) = entry.recv().await.unwrap();
            assert_eq!(first.to.to_string(), "p/next");
            assert!(workers.clone().try_acquire_many_owned(64).is_err());
            if lost_waiter == 1 {
                call.abort();
                assert!(call.await.unwrap_err().is_cancelled());
            } else if lost_waiter == 2 {
                tokio::time::pause(); // mutation is committed; only this in-memory gate is pending.
                assert!(matches!(call.await.unwrap(), Err(TaskError::Timeout)));
                tokio::time::resume();
            } else {
                // Retain the waiter; its detached JoinHandle still allows the worker to run.
                drop(call);
            }
            release.send(failed != 0).unwrap();
            for (index, audience) in [(1, "p/worker"), (2, "owner")] {
                let request = tokio::select! {
                    request = entry.recv() => request,
                    _ = workers.acquire_many(64) => panic!("fanout ended before {audience} was attempted"),
                };
                let (content, release) = request.expect("missing remaining invalidation send");
                assert_eq!(content.to.to_string(), audience);
                assert_eq!(content.kind, MessageKind::Note);
                assert!(workers.clone().try_acquire_many_owned(64).is_err());
                release.send(index != failed).unwrap();
            }
            let result = finished.await.unwrap();
            assert_eq!(result["changed"], true);
            assert_eq!(result["notification"]["status"], "unconfirmed");
            assert_eq!(result["notification"]["error"]["code"], "unavailable");
            let diagnostic = result["notification"]["error"]["message"].as_str().unwrap();
            assert!(diagnostic.contains(if failed == 0 { "p/next" } else { "p/worker" }));
            let state = org.snapshot().await.unwrap();
            assert_eq!(
                state
                    .task(&"t3".parse().unwrap())
                    .unwrap()
                    .agent_id
                    .as_ref()
                    .unwrap()
                    .to_string(),
                "p/next"
            );
            assert_eq!(state.messages().count(), 2);
            for message in state.messages() {
                assert!(
                    diagnostic.contains(&message.id.to_string()),
                    "lost queued ID: {diagnostic}"
                );
            }
            drop(workers.acquire_many_owned(64).await.unwrap());
            cancel.cancel();
        }
    }
}

// Break caught: a later failure overwrites the first error code or hides another failed/queued audience.
#[tokio::test]
async fn fanout_preserves_first_error_and_every_failed_audience() {
    let (_dir, org, cancel) = org().await;
    for (name, pane) in [("lead", "%2"), ("next", "%3")] {
        org.execute(
            OrgCommand::Adopt {
                agent_id: format!("p/{name}").parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }
    org.execute(
        OrgCommand::Task(assign(TaskActor {
            who: Address::Owner,
            lead: false,
        })),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let sender = org.clone();
    let mut attempts = Vec::new();
    let result = execute_mutation(
        org.clone(),
        CallContext {
            from_pane: Some(CallerPane("%2".into())),
            ..caller()
        },
        |by| TaskCommand::Assign {
            task_id: "t3".parse().unwrap(),
            agent_id: "p/next".parse().unwrap(),
            by,
        },
        |content| {
            attempts.push(content.to.to_string());
            let error = match attempts.len() {
                1 => Some(TaskError::Refused("first audience failed".into())),
                2 => Some(TaskError::Organisation(OrgServiceError::Unavailable(
                    "second audience failed".into(),
                ))),
                _ => None,
            };
            let sender = sender.clone();
            async move {
                match error {
                    Some(error) => Err(error),
                    None => enqueue(sender, content).await,
                }
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(attempts, ["p/next", "p/worker", "owner"]);
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(result["notification"]["status"], "unconfirmed");
    assert_eq!(
        result["notification"]["error"]["code"], "refused",
        "first failed send's code must survive later failures"
    );
    let diagnostic = result["notification"]["error"]["message"].as_str().unwrap();
    for audience in [
        "p/next (t3)",
        "p/worker (t3)",
        "first audience failed",
        "second audience failed",
    ] {
        assert!(
            diagnostic.contains(audience),
            "lost failed audience: {diagnostic}"
        );
    }
    let state = org.snapshot().await.unwrap();
    assert_eq!(state.messages().count(), 1);
    let queued = state.messages().next().unwrap();
    assert_eq!(queued.content.to, Address::Owner);
    assert!(diagnostic.contains(&queued.id.to_string()));
    cancel.cancel();
}

// Break caught: replay synthesizes a missing loss Note or repeats a completed one.
#[tokio::test]
async fn notice_only_prefix_and_completed_replay_do_not_fabricate_messages() {
    for complete in [false, true] {
        let (dir, org, cancel) = org().await;
        org.execute(
            OrgCommand::Task(assign(TaskActor {
                who: Address::Owner,
                lead: false,
            })),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        let sender = org.clone();
        let (entered, entry) = oneshot::channel();
        let mut entered = Some(entered);
        let call = tokio::spawn(execute_mutation(
            org.clone(),
            owner_caller(),
            |by| TaskCommand::SetState {
                task_id: "t3".parse().unwrap(),
                state: TaskState::Cancelled,
                reason: None,
                by,
            },
            move |content| {
                let entered = entered.take().unwrap();
                let sender = sender.clone();
                async move {
                    entered.send(()).unwrap();
                    if complete {
                        enqueue(sender, content).await
                    } else {
                        std::future::pending::<Result<MessageId, TaskError>>().await
                    }
                }
            },
        ));
        tokio::select! {
            result = entry => result.expect("missing cancellation Note enqueue"),
            _ = org.requests.closed() => panic!("actor stopped"),
        }
        if complete {
            assert!(matches!(
                call.await.unwrap().unwrap().notification,
                Some(Notification::Queued { .. })
            ));
        } else {
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
        }
        let before = org.snapshot().await.unwrap();
        assert_eq!(before.messages().count(), usize::from(complete));
        assert_eq!(
            before.task(&"t3".parse().unwrap()).unwrap().state,
            TaskState::Cancelled
        );
        let next = tempfile::tempdir().unwrap();
        copy(dir.path(), next.path());
        cancel.cancel();
        let stop = CancellationToken::new();
        let reopened = OrgHandle::open(next.path().into(), stop.clone()).await;
        assert_eq!(reopened.snapshot().await.unwrap(), before);
        stop.cancel();
    }
}

fn invalidation_context(
    base: u64,
    review_state: TaskState,
    assigned: bool,
    related: bool,
) -> (OrgState, TaskId, TaskId) {
    let mut state = verdict_snapshot(TaskState::Working, true);
    let now = chrono::Utc::now();
    let id: TaskId = format!("t{base}").parse().unwrap();
    let review: TaskId = format!("t{}", base + 4).parse().unwrap();
    let mut work = created("é".repeat(512));
    work.files = (0..40)
        .map(|n| atm_org::TaskFile {
            path: format!("old-{n}.md"),
            kind: TaskFileKind::Report,
            note: Some("é".repeat(8192)),
        })
        .collect();
    state
        .apply(base, now, &OrgEvent::TaskCreated(work))
        .unwrap();
    state
        .apply(
            base + 1,
            now,
            &OrgEvent::TaskAssigned(atm_org::TaskAssigned {
                task_id: id.clone(),
                agent_id: "p/worker".parse().unwrap(),
                by: Address::Owner,
            }),
        )
        .unwrap();
    state
        .apply(
            base + 2,
            now,
            &OrgEvent::TaskSubmitted(atm_org::TaskSubmitted {
                task_id: id.clone(),
                what: TaskSubmission::Result,
                files: vec![],
                by: "p/worker".parse().unwrap(),
            }),
        )
        .unwrap();
    state
        .apply(
            base + 3,
            now,
            &OrgEvent::AgentAdopted {
                agent_id: "p/reviewer".parse().unwrap(),
                pane: "%2".into(),
                session_id: None,
            },
        )
        .unwrap();
    let mut body = created("Review".into());
    body.kind = TaskKind::Review;
    body.judges = Some(if related {
        id.clone()
    } else {
        "t3".parse().unwrap()
    });
    state
        .apply(base + 4, now, &OrgEvent::TaskCreated(body))
        .unwrap();
    if assigned {
        state
            .apply(
                base + 5,
                now,
                &OrgEvent::TaskAssigned(atm_org::TaskAssigned {
                    task_id: review.clone(),
                    agent_id: "p/reviewer".parse().unwrap(),
                    by: Address::Owner,
                }),
            )
            .unwrap();
    }
    state
        .apply(
            base + 6,
            now,
            &OrgEvent::TaskStateChanged(atm_org::TaskStateChanged {
                task_id: review.clone(),
                state: review_state,
                reason: Some("é".repeat(8192)),
                by: Address::Owner,
            }),
        )
        .unwrap();
    state
        .apply(
            base + 7,
            now,
            &OrgEvent::TaskReviewStarted(atm_org::TaskReviewStarted {
                task_id: id.clone(),
                review_task: review.clone(),
                by: Address::Owner,
            }),
        )
        .unwrap();
    (state, id, review)
}

// Break caught: historical files/Unicode reasons overflow Notes, or superseded/current/unrelated work is woken.
#[test]
fn invalidation_bounds_and_supersession() {
    for base in [20, u64::MAX - 100] {
        let (mut state, id, review) = invalidation_context(base, TaskState::Working, true, true);
        let before = state.task(&id).unwrap().clone();
        let event = OrgEvent::TaskStateChanged(atm_org::TaskStateChanged {
            task_id: id.clone(),
            state: TaskState::Cancelled,
            reason: Some("🙂".repeat(4096)),
            by: Address::Owner,
        });
        state.apply(base + 8, chrono::Utc::now(), &event).unwrap();
        let notes = notifications(&event, Some(&before), &state).unwrap();
        assert_eq!(notes.len(), 2, "missing bounded live-review notice");
        assert_eq!(notes[0].task, Some(id.clone()));
        assert_eq!(notes[1].task, Some(review.clone()));
        for note in &notes {
            assert!(note.body.len() < 512);
            assert!(note.attachments.is_empty());
            assert!(!note.body.contains('é') && !note.body.contains('🙂'));
            assert!(note.body.contains(&format!("atm task get {id}")));
            validate_send(&SendParams {
                to: note.to.clone(),
                kind: note.kind,
                body: note.body.clone(),
                attachments: note.attachments.clone(),
                task: note.task.clone(),
            })
            .unwrap();
        }
        assert!(notes[1].body.contains(&format!("atm task get {review}")));
        // Restoring the exact linked review suppresses its obsolete invalidation.
        state
            .apply(
                base + 9,
                chrono::Utc::now(),
                &OrgEvent::TaskReviewStarted(atm_org::TaskReviewStarted {
                    task_id: id,
                    review_task: review,
                    by: Address::Owner,
                }),
            )
            .unwrap();
        assert!(notifications(&event, Some(&before), &state)
            .unwrap()
            .is_empty());
    }
    for review_state in [
        TaskState::Open,
        TaskState::Assigned,
        TaskState::Working,
        TaskState::Waiting,
        TaskState::Blocked,
        TaskState::ChangesRequested,
        TaskState::Submitted,
        TaskState::Accepted,
        TaskState::Cancelled,
        TaskState::Unknown,
    ] {
        for assigned in [false, true] {
            for related in [false, true] {
                let (mut state, id, review) =
                    invalidation_context(20, review_state, assigned, related);
                let before = state.task(&id).unwrap().clone();
                let event = OrgEvent::TaskVerdict(TaskVerdictGiven {
                    task_id: id.clone(),
                    verdict: TaskVerdict::Accepted,
                    review_task: None,
                    note: Some("é".repeat(8192)),
                    by: Address::Owner,
                });
                state.apply(28, chrono::Utc::now(), &event).unwrap();
                let messages = notifications(&event, Some(&before), &state).unwrap();
                let live = matches!(
                    review_state,
                    TaskState::Assigned
                        | TaskState::Working
                        | TaskState::Waiting
                        | TaskState::Blocked
                        | TaskState::ChangesRequested
                );
                assert_eq!(
                    messages.len(),
                    usize::from(live && assigned && related),
                    "review {review_state:?}, assigned={assigned}, related={related}"
                );
                if let Some(note) = messages.first() {
                    assert_eq!(note.task.as_ref(), Some(&review));
                    assert_eq!(note.kind, MessageKind::Note);
                }
                // A still-current linked R must never be called superseded.
                let mut current = state.clone();
                current
                    .apply(
                        29,
                        chrono::Utc::now(),
                        &OrgEvent::TaskReviewStarted(atm_org::TaskReviewStarted {
                            task_id: id.clone(),
                            review_task: review.clone(),
                            by: Address::Owner,
                        }),
                    )
                    .unwrap();
                assert!(notifications(&event, Some(&before), &current)
                    .unwrap()
                    .is_empty());
                // An unlinked child that judges T is not discovered by a scan.
                let mut unlinked = before.clone();
                unlinked.review_task = None;
                unlinked.state = TaskState::Submitted;
                assert!(notifications(&event, Some(&unlinked), &state)
                    .unwrap()
                    .is_empty());
            }
        }
    }
    let mut state = verdict_snapshot(TaskState::Working, true);
    state
        .apply(
            6,
            chrono::Utc::now(),
            &OrgEvent::AgentAdopted {
                agent_id: "p/next".parse().unwrap(),
                pane: "%2".into(),
                session_id: None,
            },
        )
        .unwrap();
    let before = state.task(&"t3".parse().unwrap()).unwrap().clone();
    let event = OrgEvent::TaskAssigned(atm_org::TaskAssigned {
        task_id: before.id.clone(),
        agent_id: "p/next".parse().unwrap(),
        by: Address::Owner,
    });
    state.apply(7, chrono::Utc::now(), &event).unwrap();
    state
        .apply(
            8,
            chrono::Utc::now(),
            &OrgEvent::TaskAssigned(atm_org::TaskAssigned {
                task_id: before.id.clone(),
                agent_id: "p/worker".parse().unwrap(),
                by: Address::Owner,
            }),
        )
        .unwrap();
    assert!(notifications(&event, Some(&before), &state)
        .unwrap()
        .iter()
        .all(|m| m.kind != MessageKind::Note));
    let mut state = verdict_snapshot(TaskState::Submitted, true);
    let before = state.task(&"t3".parse().unwrap()).unwrap().clone();
    let withdrawal = OrgEvent::TaskStateChanged(atm_org::TaskStateChanged {
        task_id: before.id.clone(),
        state: TaskState::Working,
        reason: None,
        by: "p/worker".parse().unwrap(),
    });
    state.apply(6, chrono::Utc::now(), &withdrawal).unwrap();
    assert_eq!(
        notifications(&withdrawal, Some(&before), &state)
            .unwrap()
            .len(),
        1,
        "missing withdrawal notice"
    );
    let working = state.task(&before.id).unwrap().clone();
    assert!(notifications(&withdrawal, Some(&working), &state)
        .unwrap()
        .is_empty());
    state
        .apply(
            7,
            chrono::Utc::now(),
            &OrgEvent::TaskSubmitted(atm_org::TaskSubmitted {
                task_id: before.id.clone(),
                what: TaskSubmission::Result,
                files: vec![],
                by: "p/worker".parse().unwrap(),
            }),
        )
        .unwrap();
    assert!(notifications(&withdrawal, Some(&before), &state)
        .unwrap()
        .is_empty());
}

// Break caught: missing committed context guesses a recipient or silently reports no notification.
#[tokio::test]
async fn missing_invalidation_context_is_explicitly_unconfirmed() {
    use crate::{
        eventlog::Envelope,
        org::{Request, TaskCommit},
    };
    use tokio::sync::{broadcast, watch};
    for missing_before in [false, true] {
        let (requests, mut receive) = mpsc::channel(8);
        let (_health, health) = watch::channel(Ok(5));
        let (committed, _) = broadcast::channel(8);
        let org = OrgHandle {
            requests,
            health,
            committed,
            reads: None,
        };
        let (before_state, _, _) = invalidation_context(20, TaskState::Working, true, true);
        let before = before_state.task(&"t20".parse().unwrap()).cloned();
        let call = tokio::spawn(execute_mutation(
            org,
            owner_caller(),
            |by| TaskCommand::SetState {
                task_id: "t20".parse().unwrap(),
                state: TaskState::Cancelled,
                reason: None,
                by,
            },
            |_| async { panic!("must not guess a notification audience") },
        ));
        let Request::Snapshot(reply) = receive.recv().await.unwrap() else {
            panic!("identity snapshot")
        };
        reply.send(before_state.clone()).unwrap();
        let Request::Execute { reply, now, .. } = receive.recv().await.unwrap() else {
            panic!("mutation")
        };
        let event = OrgEvent::TaskStateChanged(atm_org::TaskStateChanged {
            task_id: "t20".parse().unwrap(),
            state: TaskState::Cancelled,
            reason: None,
            by: Address::Owner,
        });
        reply
            .send(Ok(TaskCommit {
                before: if missing_before { None } else { before },
                events: vec![Envelope {
                    seq: 28,
                    ts: now,
                    kind: event.kind().into(),
                    body: event.to_body(),
                }],
            }))
            .unwrap();
        let Request::Snapshot(reply) = receive.recv().await.unwrap() else {
            panic!("post-commit snapshot")
        };
        let mut current = if missing_before {
            before_state
        } else {
            // Preserve T but omit the linked R from the supplied snapshot.
            let mut current = verdict_snapshot(TaskState::Working, true);
            current
                .apply(20, now, &OrgEvent::TaskCreated(created("Work".into())))
                .unwrap();
            current
        };
        current.apply(28, now, &event).unwrap();
        reply.send(current).unwrap();
        let result = serde_json::to_value(call.await.unwrap().unwrap()).unwrap();
        assert_eq!(result["changed"], true);
        assert_eq!(
            result["notification"]["status"], "unconfirmed",
            "missing context must be explicit"
        );
        assert_eq!(
            result["notification"]["error"]["code"],
            if missing_before {
                "internal"
            } else {
                "not_found"
            }
        );
    }
}

// Break caught: answer copies an oversized note/stale files, loses review access, or guesses an assignee.
#[test]
fn verdict_answer_reads_note_and_review_by_reference() {
    for (choice, state_kind, word) in [
        (TaskVerdict::Accepted, TaskState::Working, "accepted"),
        (
            TaskVerdict::ChangesRequested,
            TaskState::ChangesRequested,
            "changes_requested",
        ),
    ] {
        for reviewed in [false, true] {
            for note in [None, Some("Checked é🙂".into()), Some("é".repeat(8192))] {
                let mut state = verdict_snapshot(state_kind, true);
                let now = chrono::Utc::now();
                let mut review = created("Review".into());
                review.kind = TaskKind::Review;
                review.judges = Some("t3".parse().unwrap());
                state.apply(6, now, &OrgEvent::TaskCreated(review)).unwrap();
                for seq in 7..87 {
                    state
                        .apply(
                            seq,
                            now,
                            &OrgEvent::TaskAttached(atm_org::TaskAttached {
                                task_id: if seq < 47 { "t3" } else { "t6" }.parse().unwrap(),
                                by: Address::Owner,
                                file: atm_org::TaskFile {
                                    path: format!("evidence-{seq}.md"),
                                    kind: TaskFileKind::Report,
                                    note: None,
                                },
                            }),
                        )
                        .unwrap();
                }
                let mut fact = verdict_fact(choice);
                fact.note = note;
                fact.review_task = reviewed.then(|| "t6".parse().unwrap());
                let answer = notification(&OrgEvent::TaskVerdict(fact), &state)
                    .unwrap()
                    .expect("missing verdict answer");
                let mut body = format!("Task t3 verdict recorded: {word}.\nRe-read the current task and verdict note before acting: atm task get t3\nResume only as its current state and approved brief allow.");
                if reviewed {
                    body.push_str("\nReview evidence: atm task get t6");
                }
                assert_eq!(
                    answer,
                    MessageContent {
                        from: Address::Owner,
                        to: "p/worker".parse().unwrap(),
                        kind: MessageKind::Answer,
                        body,
                        attachments: vec![],
                        task: Some("t3".parse().unwrap()),
                    }
                );
                validate_send(&SendParams {
                    to: answer.to,
                    kind: answer.kind,
                    body: answer.body,
                    attachments: answer.attachments,
                    task: answer.task,
                })
                .unwrap();
            }
        }
        assert!(matches!(
            notification(
                &OrgEvent::TaskVerdict(verdict_fact(choice)),
                &verdict_snapshot(state_kind, false)
            ),
            Err(TaskError::Internal(_))
        ));
    }
    for state in [
        TaskState::Open,
        TaskState::Assigned,
        TaskState::Waiting,
        TaskState::Blocked,
        TaskState::Submitted,
        TaskState::InReview,
        TaskState::Accepted,
        TaskState::Cancelled,
        TaskState::Unknown,
    ] {
        for choice in [
            TaskVerdict::Accepted,
            TaskVerdict::ChangesRequested,
            TaskVerdict::Unknown,
        ] {
            assert!(notification(
                &OrgEvent::TaskVerdict(verdict_fact(choice)),
                &verdict_snapshot(state, true)
            )
            .unwrap()
            .is_none());
        }
    }
    for (choice, state) in [
        (TaskVerdict::Accepted, TaskState::ChangesRequested),
        (TaskVerdict::ChangesRequested, TaskState::Working),
        (TaskVerdict::Unknown, TaskState::Working),
    ] {
        assert!(notification(
            &OrgEvent::TaskVerdict(verdict_fact(choice)),
            &verdict_snapshot(state, true)
        )
        .unwrap()
        .is_none());
    }
}

// Break caught: composition uses the pre-command snapshot, or reports None as an enqueue error.
#[tokio::test]
async fn superseded_verdict_snapshot_does_not_emit_stale_wake() {
    use crate::{eventlog::Envelope, org::Request};
    use tokio::sync::{broadcast, watch};
    for choice in [TaskVerdict::Accepted, TaskVerdict::ChangesRequested] {
        let eligible = if choice == TaskVerdict::Accepted {
            TaskState::Working
        } else {
            TaskState::ChangesRequested
        };
        for state_kind in [
            eligible,
            TaskState::Submitted,
            TaskState::InReview,
            TaskState::Waiting,
            TaskState::Blocked,
            TaskState::Assigned,
            TaskState::Open,
            TaskState::Cancelled,
            TaskState::Accepted,
            TaskState::Unknown,
        ] {
            let (requests, mut receive) = mpsc::channel(8);
            let (_health, health) = watch::channel(Ok(5));
            let (committed, _) = broadcast::channel(8);
            let org = OrgHandle {
                requests,
                health,
                committed,
                reads: None,
            };
            let mut call = tokio::spawn(execute_mutation(
                org,
                owner_caller(),
                move |by| verdict(by, choice),
                |content| async move {
                    assert_eq!(content.to.to_string(), "p/worker");
                    Ok(MessageId::new(99))
                },
            ));
            let Request::Snapshot(reply) = receive.recv().await.unwrap() else {
                panic!("expected identity snapshot")
            };
            reply
                .send(verdict_snapshot(TaskState::Submitted, true))
                .unwrap();
            let Request::Execute { reply, now, .. } = receive.recv().await.unwrap() else {
                panic!("expected verdict")
            };
            let event = OrgEvent::TaskVerdict(verdict_fact(choice));
            reply
                .send(Ok(crate::org::TaskCommit {
                    before: verdict_snapshot(TaskState::Submitted, true)
                        .task(&"t3".parse().unwrap())
                        .cloned(),
                    events: vec![Envelope {
                        seq: 6,
                        ts: now,
                        kind: event.kind().into(),
                        body: event.to_body(),
                    }],
                }))
                .unwrap();
            let request = tokio::select! {
                request = receive.recv() => request,
                result = &mut call => panic!("missing verdict answer composition: {:?}", result.unwrap().unwrap()),
            };
            let Some(Request::Snapshot(reply)) = request else {
                panic!("missing verdict answer composition: no fresh post-verdict snapshot")
            };
            // A reassignment is represented by Assigned, not the old Working snapshot.
            reply.send(verdict_snapshot(state_kind, true)).unwrap();
            let result = serde_json::to_value(call.await.unwrap().unwrap()).unwrap();
            assert_eq!(result["changed"], true);
            if state_kind == eligible {
                assert_eq!(
                    result["notification"],
                    serde_json::json!({"status":"queued","message_id":"m99"})
                );
            } else {
                assert_eq!(result["notification"], serde_json::Value::Null);
            }
        }
    }
}

// Break caught: enqueue failure hides/undoes a committed verdict or calls its answer a report.
#[tokio::test]
async fn verdict_enqueue_failure_preserves_committed_state() {
    for (choice, wanted) in [
        (TaskVerdict::Accepted, TaskState::Working),
        (TaskVerdict::ChangesRequested, TaskState::ChangesRequested),
    ] {
        let (_dir, org, cancel) = verdict_org().await;
        let result = execute_mutation(
            org.clone(),
            owner_caller(),
            move |by| verdict(by, choice),
            |_| async {
                Err(TaskError::Organisation(OrgServiceError::Unavailable(
                    "unknown append outcome".into(),
                )))
            },
        )
        .await
        .unwrap();
        let result = serde_json::to_value(result).unwrap();
        assert_eq!(result["changed"], true);
        assert_eq!(
            result["notification"]["status"], "unconfirmed",
            "missing verdict answer enqueue"
        );
        assert_eq!(result["notification"]["error"]["code"], "unavailable");
        assert!(result["notification"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("task t3 committed; automatic answer enqueue was not confirmed"));
        let state = org.snapshot().await.unwrap();
        assert_eq!(state.task(&"t3".parse().unwrap()).unwrap().state, wanted);
        assert_eq!(state.messages().count(), 0);
        cancel.cancel();
    }
}

// Break caught: dropping/timing out a verdict waiter drops its retained enqueue or permit.
#[tokio::test]
async fn verdict_worker_retains_enqueue_after_waiter_loss() {
    for choice in [TaskVerdict::Accepted, TaskVerdict::ChangesRequested] {
        for timeout in [false, true] {
            let (_dir, org, cancel) = verdict_org().await;
            let tasks = TaskHandle::new(org.clone());
            let workers = tasks.workers.clone();
            let (entered, entry) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            let (done, finished) = oneshot::channel();
            let sender = org.clone();
            let mut gate = Some((entered, gate, done));
            let call = tokio::spawn(async move {
                tasks
                    .run(execute_mutation(
                        tasks.org.clone(),
                        owner_caller(),
                        move |by| verdict(by, choice),
                        move |content| {
                            let (entered, gate, done) = gate.take().unwrap();
                            let sender = sender.clone();
                            async move {
                                entered.send(()).unwrap();
                                gate.await.unwrap();
                                let result = enqueue(sender, content).await;
                                done.send(()).unwrap();
                                result
                            }
                        },
                    ))
                    .await
            });
            entry.await.expect("missing verdict answer enqueue");
            if timeout {
                // Only an in-memory enqueue gate is pending; real log I/O is complete.
                tokio::time::pause();
                assert!(matches!(call.await.unwrap(), Err(TaskError::Timeout)));
                tokio::time::resume();
            } else {
                call.abort();
                assert!(call.await.unwrap_err().is_cancelled());
            }
            assert!(workers.clone().try_acquire_many_owned(64).is_err());
            release.send(()).unwrap();
            finished.await.unwrap();
            drop(workers.acquire_many_owned(64).await.unwrap());
            let state = org.snapshot().await.unwrap();
            assert_eq!(
                state.task(&"t3".parse().unwrap()).unwrap().state,
                if choice == TaskVerdict::Accepted {
                    TaskState::Working
                } else {
                    TaskState::ChangesRequested
                }
            );
            let messages: Vec<_> = state.messages().collect();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content.kind, MessageKind::Answer);
            assert_eq!(messages[0].content.task.as_ref().unwrap().to_string(), "t3");
            cancel.cancel();
        }
    }
}

// Break caught: replay fabricates a lost verdict wake or duplicates an already committed answer.
#[tokio::test]
async fn verdict_prefix_and_answer_replay_do_not_fabricate_messages() {
    for complete in [false, true] {
        let (dir, org, cancel) = verdict_org().await;
        if complete {
            let sender = org.clone();
            let result = execute_mutation(
                org.clone(),
                owner_caller(),
                |by| verdict(by, TaskVerdict::Accepted),
                move |content| enqueue(sender.clone(), content),
            )
            .await
            .unwrap();
            assert!(
                matches!(result.notification, Some(Notification::Queued { .. })),
                "missing verdict answer"
            );
        } else {
            let (entered, entry) = oneshot::channel();
            let mut entered = Some(entered);
            let call = tokio::spawn(execute_mutation(
                org.clone(),
                owner_caller(),
                |by| verdict(by, TaskVerdict::Accepted),
                move |_| {
                    let entered = entered.take().unwrap();
                    async move {
                        entered.send(()).unwrap();
                        std::future::pending::<Result<MessageId, TaskError>>().await
                    }
                },
            ));
            entry.await.expect("missing verdict answer enqueue");
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
        }
        let before = org.snapshot().await.unwrap();
        assert_eq!(
            before.task(&"t3".parse().unwrap()).unwrap().state,
            TaskState::Working
        );
        assert_eq!(before.messages().count(), usize::from(complete));
        let next = tempfile::tempdir().unwrap();
        copy(dir.path(), next.path());
        cancel.cancel();
        let stop = CancellationToken::new();
        let reopened = OrgHandle::open(next.path().into(), stop.clone()).await;
        assert_eq!(reopened.snapshot().await.unwrap(), before);
        stop.cancel();
    }
}

// Break caught: an internally created task addresses its report to system, which has no inbox.
#[test]
fn system_created_task_report_falls_back_to_owner() {
    let mut state = OrgState::default();
    let mut task = created("Work".into());
    task.creator = Address::System;
    state
        .apply(1, chrono::Utc::now(), &OrgEvent::TaskCreated(task))
        .unwrap();
    let event = OrgEvent::TaskSubmitted(atm_org::TaskSubmitted {
        task_id: "t1".parse().unwrap(),
        what: TaskSubmission::Result,
        files: vec![],
        by: "p/worker".parse().unwrap(),
    });
    let report = notification(&event, &state).unwrap().unwrap();
    assert_eq!(report.to, Address::Owner);
    assert!(report
        .body
        .ends_with("\nCreator is system; report routed to owner."));
}

// Break caught: waiter cancellation drops a post-commit enqueue or releases its permit early.
#[tokio::test]
async fn cancelled_waiter_leaves_worker_to_finish_enqueue_exactly_once() {
    retained_worker(false).await;
}
// Break caught: the service timeout aborts admitted work or permits replacement workers.
#[tokio::test]
async fn timed_out_waiter_leaves_worker_to_finish_enqueue_exactly_once() {
    retained_worker(true).await;
}
async fn retained_worker(timeout: bool) {
    let (_dir, org, cancel) = org().await;
    let tasks = TaskHandle::new(org.clone());
    let workers = tasks.workers.clone();
    let (entered, entry) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    let sender = org.clone();
    let mut gate = Some((entered, gate, done));
    let call = tokio::spawn(async move {
        tasks
            .run(execute_mutation(
                tasks.org.clone(),
                caller(),
                assign,
                move |content| {
                    let (entered, gate, done) = gate.take().unwrap();
                    let sender = sender.clone();
                    async move {
                        entered.send(()).unwrap();
                        gate.await.unwrap();
                        let result = enqueue(sender, content).await;
                        done.send(()).unwrap();
                        result
                    }
                },
            ))
            .await
    });
    entry.await.unwrap();
    if timeout {
        // All real log I/O has completed; only an in-memory gate is in flight.
        tokio::time::pause();
        assert!(matches!(call.await.unwrap(), Err(TaskError::Timeout)));
        tokio::time::resume();
    } else {
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
    }
    assert!(workers.clone().try_acquire_many_owned(64).is_err());
    release.send(()).unwrap();
    finished.await.unwrap();
    drop(workers.acquire_many_owned(64).await.unwrap());
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state.task(&"t3".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    let messages: Vec<_> = state.messages().collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.task.as_ref().unwrap().to_string(), "t3");
    cancel.cancel();
}

// Break caught: admission parks more callers, or cancellation creates more than 64 workers.
#[tokio::test]
async fn full_pool_refuses_the_next_mutation_without_waiting_for_a_slot() {
    let tasks = TaskHandle::new(OrgHandle::disabled("not used by admission"));
    let release = CancellationToken::new();
    let (entered, mut entry) = mpsc::unbounded_channel();
    let mut calls = Vec::new();
    for _ in 0..64 {
        let tasks = tasks.clone();
        let gate = release.clone();
        let entered = entered.clone();
        calls.push(tokio::spawn(async move {
            tasks
                .run(async move {
                    entered.send(()).unwrap();
                    gate.cancelled().await;
                    Ok(())
                })
                .await
        }));
    }
    for _ in 0..64 {
        entry.recv().await.unwrap();
    }
    assert!(matches!(
        tasks.run(async { Ok(()) }).await,
        Err(TaskError::Unavailable(_))
    ));
    release.cancel();
    for call in calls {
        call.await.unwrap().unwrap();
    }
    tasks.run(async { Ok(()) }).await.unwrap();
}

// Break caught: enqueue failure hides the committed task or claims no effect/delivery.
#[tokio::test]
async fn enqueue_failure_is_a_partial_result_not_a_failed_transition() {
    let (_dir, org, cancel) = org().await;
    let result = execute_mutation(org.clone(), caller(), assign, |_| async {
        Err(TaskError::Organisation(OrgServiceError::Unavailable(
            "unknown append outcome".into(),
        )))
    })
    .await
    .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["task_id"], "t3");
    assert_eq!(result["changed"], true);
    assert_eq!(result["notification"]["status"], "unconfirmed");
    assert_eq!(result["notification"]["error"]["code"], "unavailable");
    assert!(result["notification"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("task t3 committed"));
    let state = org.snapshot().await.unwrap();
    assert_eq!(
        state.task(&"t3".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    assert_eq!(state.messages().count(), 0);
    cancel.cancel();
}

// Break caught: restart synthesizes messages for a task-only durable prefix, contrary to the approved limit.
#[tokio::test]
async fn task_only_durable_prefix_replays_without_fabricating_a_message() {
    let (dir, org, cancel) = org().await;
    let (entered, entry) = oneshot::channel();
    let mut entered = Some(entered);
    let task = tokio::spawn(execute_mutation(org.clone(), caller(), assign, move |_| {
        let entered = entered.take().unwrap();
        async move {
            entered.send(()).unwrap();
            std::future::pending::<Result<MessageId, TaskError>>().await
        }
    }));
    entry.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // Copy the real committed log while no append is in flight, giving the
    // next lifetime its own stream lock, as message persistence tests do.
    let next = tempfile::tempdir().unwrap();
    copy(dir.path(), next.path());
    cancel.cancel();
    let stop = CancellationToken::new();
    let reopened = OrgHandle::open(next.path().into(), stop.clone()).await;
    let state = reopened.snapshot().await.unwrap();
    assert_eq!(
        state.task(&"t3".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    assert_eq!(state.messages().count(), 0);
    stop.cancel();
}
fn copy(from: &std::path::Path, to: &std::path::Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

// Break caught: list pagination uses lexicographic IDs, silently truncates, or emits empty continuations.
#[test]
fn list_byte_and_count_pages_preserve_numeric_order_and_full_entries() {
    let mut state = OrgState::default();
    let now = chrono::Utc::now();
    for seq in 1..=600 {
        state
            .apply(seq, now, &OrgEvent::TaskCreated(created("é".repeat(512))))
            .unwrap();
    }
    let p: ListParams = serde_json::from_value(serde_json::json!({"limit":1000})).unwrap();
    let page = list(&state, &p).unwrap();
    assert!(page.tasks.len() > 400 && page.tasks.len() < 600);
    assert!(encoded_size(&page).unwrap() <= PAGE_BYTES);
    assert_eq!(page.tasks.get(8).unwrap().task_id.to_string(), "t9");
    assert_eq!(page.tasks.get(9).unwrap().task_id.to_string(), "t10");
    let after = page.next_after.clone().unwrap();
    let p: ListParams =
        serde_json::from_value(serde_json::json!({"after":after,"limit":1000})).unwrap();
    let rest = list(&state, &p).unwrap();
    assert_eq!(page.tasks.len() + rest.tasks.len(), 600);
    assert!(rest.next_after.is_none());
    let p: ListParams =
        serde_json::from_value(serde_json::json!({"after":"t8","limit":2})).unwrap();
    let page = list(&state, &p).unwrap();
    assert_eq!(
        page.tasks
            .iter()
            .map(|t| t.task_id.to_string())
            .collect::<Vec<_>>(),
        ["t9", "t10"]
    );
    assert_eq!(page.next_after.unwrap().to_string(), "t10");
    let mut huge = OrgState::default();
    huge.apply(
        1,
        now,
        &OrgEvent::TaskCreated(created("x".repeat(PAGE_BYTES))),
    )
    .unwrap();
    assert!(matches!(list(&huge,&p),Ok(ListResult { tasks, .. }) if tasks.is_empty()));
    let p: ListParams = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(matches!(list(&huge, &p), Err(TaskError::Refused(_))));
}
