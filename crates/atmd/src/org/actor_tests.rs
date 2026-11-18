use super::*;
use crate::org::OrgHandle;
use atm_org::{AgentStatus, OrgEvent};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::oneshot;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn project() -> OrgCommand {
    OrgCommand::RegisterProject {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
        move_root: false,
    }
}

fn spawn<F, Fut>(append: F) -> (OrgHandle, CancellationToken, tokio::task::JoinHandle<()>)
where
    F: FnMut(Envelope) -> Fut + Send + 'static,
    Fut: Future<Output = Result<u64, EventLogError>> + Send + 'static,
{
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(0));
    let (committed, _) = broadcast::channel(1024);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(
        OrgState::default(),
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        append,
    ));
    (
        OrgHandle {
            requests,
            health: status,
            committed,
            reads: None,
        },
        cancel,
        task,
    )
}

// Break caught: data and cursor are sampled in different actor turns, or
// replay loses the sequence high-water of an unknown event.
#[tokio::test]
async fn snapshot_at_matches_fold_and_replay_cursor() {
    if crate::eventlog::test_support::isolated(
        "org::actor::tests::snapshot_at_matches_fold_and_replay_cursor",
    ) {
        return;
    }
    let mut seq = 0;
    let (org, cancel, task) = spawn(move |_| {
        seq += 1;
        std::future::ready(Ok(seq))
    });
    org.execute(project(), now()).await.unwrap();
    let (before, cursor) = org.snapshot_at().await.unwrap();
    assert_eq!(cursor, 1);
    assert_eq!(before.projects().count(), 1);
    org.execute(
        OrgCommand::Adopt {
            agent_id: "demo/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
        now(),
    )
    .await
    .unwrap();
    assert_eq!(before.agents().count(), 0);
    let (after, cursor) = org.snapshot_at().await.unwrap();
    assert_eq!(cursor, 2);
    assert_eq!(after.agents().count(), 1);
    cancel.cancel();
    task.await.unwrap();
    assert!(org.snapshot_at().await.is_err());
    assert!(OrgHandle::disabled("unavailable")
        .snapshot_at()
        .await
        .is_err());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let log = crate::org::storage::detached_open(Duration::from_secs(60), move || {
        crate::eventlog::EventLog::open(&path, crate::eventlog::Stream::Events)
            .map_err(|e| e.to_string())
    })
    .await
    .unwrap();
    log.append(
        now(),
        "project.registered",
        serde_json::json!({"project_id":"demo","root":"/demo"}),
    )
    .await
    .unwrap();
    log.append(now(), "future.event", serde_json::json!({"ignored":true}))
        .await
        .unwrap();
    let mut closed = log.subscribe();
    drop(log);
    // Last-handle Drop only asks the detached writer to stop. Closed fences
    // this writer releasing its stream fd; the isolated child excludes
    // unrelated forks holding inherited copies.
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(60), closed.recv())
            .await
            .unwrap(),
        Err(broadcast::error::RecvError::Closed)
    ));
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let (state, cursor) = org.snapshot_at().await.unwrap();
    assert_eq!(state.projects().count(), 1);
    assert_eq!(cursor, 2);
    cancel.cancel();
}

// Break caught: reporting reads status and state in separate turns, or exposes
// a cursor for an append before the corresponding state has been applied.
#[tokio::test]
async fn report_snapshot_pairs_the_committed_cursor_and_state_in_one_actor_turn() {
    let (entered, mut entry) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let mut seq = 0;
    let (org, cancel, task) = spawn(move |_| {
        seq += 1;
        let seq = seq;
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            entered.send(seq).unwrap();
            gate.acquire().await.unwrap().forget();
            Ok(seq)
        }
    });
    let caller = org.clone();
    let first = tokio::spawn(async move { caller.execute(project(), now()).await });
    assert_eq!(entry.recv().await, Some(1));
    let mut response = Box::pin(org.report_snapshot());
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(response.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let caller = org.clone();
    let second = tokio::spawn(async move {
        caller
            .execute(
                OrgCommand::RegisterProject {
                    project_id: "other".parse().unwrap(),
                    root: "/other".into(),
                    move_root: false,
                },
                now(),
            )
            .await
    });
    release.add_permits(1);
    assert_eq!(entry.recv().await, Some(2)); // snapshot reply has already been sent
    assert_eq!(org.status().unwrap(), 1);
    first.await.unwrap().unwrap();
    release.add_permits(1);
    second.await.unwrap().unwrap();
    // Consume the queued snapshot only AFTER the next command commits. Reading
    // status on either side of a separate snapshot now gives the wrong cursor.
    let (seq, state) = response.await.unwrap();
    assert_eq!(seq, 1);
    assert_eq!(
        state
            .projects()
            .map(|p| p.id.to_string())
            .collect::<Vec<_>>(),
        ["demo"]
    );
    let (seq, state) = org.report_snapshot().await.unwrap();
    assert_eq!(seq, 2);
    assert_eq!(state.projects().count(), 2);
    cancel.cancel();
    task.await.unwrap();
}

// Characterization: accepted parent accounting is inserted before review closure.
// A reporting fold that assumes adjacent verdicts would count two owner step-ins.
#[tokio::test]
async fn parent_verdict_accounting_precedes_the_paired_review_closure() {
    use atm_org::{TaskActor, TaskCommand, TaskSubmission, TaskVerdict};
    use serde_json::json;
    let mut seq = 0;
    let (org, cancel, actor) = spawn(move |_| {
        seq += 1;
        std::future::ready(Ok(seq))
    });
    org.execute(project(), now()).await.unwrap();
    for (id, pane) in [("demo/worker", "%1"), ("demo/reviewer", "%2")] {
        org.execute(
            OrgCommand::Adopt {
                agent_id: id.parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            },
            now(),
        )
        .await
        .unwrap();
    }
    let by = |who: &str| TaskActor {
        who: who.parse().unwrap(),
        manager_of: None,
    };
    for (kind, judges, id, agent) in [
        ("work", None, "t4", "demo/worker"),
        ("review", Some("t4"), "t7", "demo/reviewer"),
    ] {
        let created = serde_json::from_value(json!({"title":"Work", "kind":kind, "creator":"owner", "judges":judges, "measure":{"kind":"owner"}, "files":[]})).unwrap();
        org.execute_task(TaskCommand::Create(created), now())
            .await
            .unwrap();
        org.execute_task(
            TaskCommand::Assign {
                task_id: id.parse().unwrap(),
                agent_id: agent.parse().unwrap(),
                by: by("owner"),
            },
            now(),
        )
        .await
        .unwrap();
        org.execute_task(
            TaskCommand::Submit {
                task_id: id.parse().unwrap(),
                what: TaskSubmission::Result,
                files: vec![],
                by: by(agent),
            },
            now(),
        )
        .await
        .unwrap();
    }
    org.execute_task(
        TaskCommand::StartReview {
            task_id: "t4".parse().unwrap(),
            review_task: "t7".parse().unwrap(),
            by: by("owner"),
        },
        now(),
    )
    .await
    .unwrap();
    let receipt = org
        .execute_task(
            TaskCommand::Verdict {
                task_id: "t4".parse().unwrap(),
                verdict: TaskVerdict::Accepted,
                review_task: Some("t7".parse().unwrap()),
                note: None,
                by: by("owner"),
            },
            now(),
        )
        .await
        .unwrap();
    assert_eq!(
        receipt
            .events
            .iter()
            .map(|e| (e.kind.as_str(), e.body["task_id"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("task.verdict", "t4"),
            ("task.cost_recorded", "t4"),
            ("task.verdict", "t7"),
            ("task.cost_recorded", "t7")
        ]
    );
    cancel.cancel();
    actor.await.unwrap();
}

// Break caught: task receipts capture a post-state, omit a no-op before-image, or alter public execute.
#[tokio::test]
async fn task_receipt_captures_the_deciding_actor_before_image() {
    use atm_org::{TaskActor, TaskCommand, TaskState};
    let mut seq = 0;
    let (org, cancel, task) = spawn(move |_| {
        seq += 1;
        std::future::ready(Ok(seq))
    });
    org.execute(project(), now()).await.unwrap();
    org.execute(
        OrgCommand::Adopt {
            agent_id: "demo/worker".parse().unwrap(),
            pane: "%1".into(),
            session_id: None,
        },
        now(),
    )
    .await
    .unwrap();
    let created = serde_json::from_value(serde_json::json!({"title":"Work","kind":"work","creator":"owner","parent":null,"judges":null,"measure":{"kind":"owner"},"budget":null,"files":[],"rests_on":null})).unwrap();
    let receipt = org
        .execute_task(TaskCommand::Create(created), now())
        .await
        .unwrap();
    assert!(receipt.before.is_none());
    assert_eq!(receipt.events.len(), 1);
    let before = org
        .snapshot()
        .await
        .unwrap()
        .task(&"t3".parse().unwrap())
        .unwrap()
        .clone();
    let command = TaskCommand::Assign {
        task_id: "t3".parse().unwrap(),
        agent_id: "demo/worker".parse().unwrap(),
        by: TaskActor {
            who: "owner".parse().unwrap(),
            manager_of: None,
        },
    };
    let receipt = org.execute_task(command.clone(), now()).await.unwrap();
    assert_eq!(receipt.before, Some(before));
    assert_eq!(receipt.events[0].kind, "task.assigned");
    let before = org
        .snapshot()
        .await
        .unwrap()
        .task(&"t3".parse().unwrap())
        .unwrap()
        .clone();
    assert_eq!(before.state, TaskState::Assigned);
    let receipt = org.execute_task(command, now()).await.unwrap();
    assert_eq!(receipt.before, Some(before));
    assert!(receipt.events.is_empty());
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn pending_append_is_not_replayed_or_released_live_before_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let log = crate::org::storage::detached_open(Duration::from_secs(60), move || {
        crate::eventlog::EventLog::open(&path, crate::eventlog::Stream::Events)
            .map_err(|e| e.to_string())
    })
    .await
    .unwrap();
    let (entered, mut entry) = mpsc::channel(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let disk = log.clone();
    let (mut org, cancel, task) = spawn(move |record| {
        let log = disk.clone();
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            let seq = log.append(record.ts, &record.kind, record.body).await?;
            entered.send(()).await.unwrap();
            let _permit = gate.acquire().await.unwrap();
            Ok(seq)
        }
    });
    org.reads = Some(crate::org::watch::ReadAccess {
        reader: crate::org::storage::Reader::start(dir.path().into()).unwrap(),
    });
    let mut watch = org.watch(vec![], 0).unwrap();
    let caller = org.clone();
    let command = tokio::spawn(async move { caller.execute(project(), now()).await });
    entry.recv().await.unwrap();
    assert_eq!(
        crate::eventlog::read_from(dir.path(), crate::eventlog::Stream::Events, 0).count(),
        1
    );
    assert!(org
        .tail(crate::org::TailParams::default())
        .await
        .unwrap()
        .is_empty());
    tokio::select! {
        biased;
        result = watch.next() => panic!("unapplied event released: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    // Cancelling an idle next must not lose the later committed publication.
    release.add_permits(1);
    command.await.unwrap().unwrap();
    assert_eq!(watch.next().await.unwrap().unwrap().seq, 1);
    assert_eq!(
        org.tail(crate::org::TailParams::default())
            .await
            .unwrap()
            .len(),
        1
    );
    cancel.cancel();
    task.await.unwrap();
    assert!(
        watch.next().await.is_err(),
        "stopped actor must invalidate a quiet watch"
    );
}

// A watch opened after the writer broadcasts but before apply must not lose
// that record. Only the append acknowledgment is gated; disk and actor are real.
#[tokio::test]
async fn watch_opened_between_broadcast_and_apply_misses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let log = crate::org::storage::detached_open(Duration::from_secs(60), move || {
        crate::eventlog::EventLog::open(&path, crate::eventlog::Stream::Events)
            .map_err(|e| e.to_string())
    })
    .await
    .unwrap();
    let (entered, mut entry) = mpsc::channel(4);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let disk = log.clone();
    let (mut org, cancel, task) = spawn(move |record| {
        let log = disk.clone();
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            let seq = log.append(record.ts, &record.kind, record.body).await?;
            entered.send(()).await.unwrap();
            let _permit = gate.acquire().await.unwrap();
            Ok(seq)
        }
    });
    org.reads = Some(crate::org::watch::ReadAccess {
        reader: crate::org::storage::Reader::start(dir.path().into()).unwrap(),
    });
    let caller = org.clone();
    let first = tokio::spawn(async move { caller.execute(project(), now()).await });
    entry.recv().await.unwrap(); // seq 1 is on disk and broadcast, but not applied
    assert_eq!(org.status().unwrap(), 0);
    let mut watch = org.watch(vec![], 0).unwrap();
    let mut quiet_watch = org.watch(vec![], 0).unwrap();
    release.add_permits(1);
    first.await.unwrap().unwrap();
    // Publication must deliver even if no later event exposes a sequence gap.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(60), quiet_watch.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .seq,
        1
    );
    let caller = org.clone();
    let second = tokio::spawn(async move {
        caller
            .execute(
                OrgCommand::ControlSent(atm_org::ControlAudit {
                    pane: "%1".into(),
                    action: serde_json::json!({"n": 2}),
                }),
                now(),
            )
            .await
    });
    entry.recv().await.unwrap();
    release.add_permits(1);
    second.await.unwrap().unwrap();
    let got = tokio::time::timeout(Duration::from_secs(60), watch.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        got.seq, 1,
        "watch skipped seq 1 and delivered seq {}",
        got.seq
    );
    assert_eq!(watch.next().await.unwrap().unwrap().seq, 2);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn failed_or_hung_events_append_keeps_socket_tracking_and_ping_live() {
    use crate::server::{
        methods::{router, MethodServices},
        ConnectionHandler,
    };
    use atm_protocol::{ClientMessage, DaemonMessage};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for fail in [false, true] {
        let (entered, mut entry) = mpsc::channel(8);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let gate = release.clone();
        let (org, org_cancel, actor) = spawn(move |_| {
            let entered = entered.clone();
            let gate = gate.clone();
            async move {
                entered.send(()).await.unwrap();
                if fail {
                    return Err(EventLogError::Closed);
                }
                let _permit = gate.acquire().await.unwrap();
                Ok(1)
            }
        });
        let registry = crate::registry::spawn_registry();
        let observer = crate::org::SummaryObserver::spawn(registry.clone(), org.clone());
        let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = ours.into_split();
        let connection_cancel = CancellationToken::new();
        let methods = Arc::new(router(MethodServices::new(registry.clone())).unwrap());
        let handler = ConnectionHandler::new(
            read,
            write,
            registry.clone(),
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            1,
            connection_cancel.clone(),
            methods,
        )
        .with_org(org.clone());
        let task = tokio::spawn(handler.run());
        let (read, mut write) = peer.into_split();
        let mut read = BufReader::new(read);
        let fixture =
            include_str!("../../../atm-pi-adapter/tests/fixtures/pi_session_cost_0.73.1.jsonl");
        let mut frames: Vec<serde_json::Value> = fixture
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for frame in &mut frames {
            frame["data"]["pid"] = serde_json::json!(std::process::id());
        }
        let probe = async {
            write
                .write_all(
                    format!(
                        "{}\n{}\n",
                        serde_json::to_string(&ClientMessage::connect(None)).unwrap(),
                        frames[0]
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Connected { .. }
            ));
            entry.recv().await.unwrap(); // summary append has failed or is stuck
            for frame in frames
                .iter()
                .skip(1)
                .filter(|frame| frame["data"]["event"] != "session_shutdown")
            {
                write
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .unwrap();
            }
            write
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(&ClientMessage::ping(42)).unwrap()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Pong { seq: 42 }
            ));
            let id = atm_core::SessionId::new(frames[0]["data"]["session_id"].as_str().unwrap());
            let session = registry.get_session(id).await.unwrap();
            assert_eq!(session.cost_usd, 0.01065);
            assert_eq!(
                session.first_prompt.as_deref(),
                Some("Read note.txt and reply.")
            );
        };
        tokio::time::timeout(Duration::from_secs(60), probe)
            .await
            .unwrap();
        release.add_permits(1);
        observer.finish().await;
        connection_cancel.cancel();
        org_cancel.cancel();
        task.await.unwrap();
        actor.await.unwrap();
    }
}

// Deliberately control only the disk boundary; decisions/apply/queue are real.
#[tokio::test]
async fn cancelled_reply_does_not_cancel_command_or_publish_early() {
    let (entered, mut entry) = mpsc::channel(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let (org, cancel, task) = spawn(move |_| {
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            entered.send(()).await.unwrap();
            let _permit = gate.acquire().await.unwrap();
            Ok(1)
        }
    });
    let caller = org.clone();
    let caller = tokio::spawn(async move { caller.execute(project(), now()).await });
    entry.recv().await.unwrap();
    assert_eq!(org.status().unwrap(), 0, "pending sync is not committed");
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    assert_eq!(org.status().unwrap(), 1);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn write_errors_stop_decisions_and_state_reads_until_restart() {
    for error in [
        EventLogError::Io {
            path: "events".into(),
            source: std::io::Error::other("disk failed"),
        },
        EventLogError::WriteOutcomeUnknown {
            path: "events".into(),
            source: std::io::Error::other("sync failed"),
        },
        EventLogError::TimeoutAfterHandover,
        EventLogError::Closed,
        EventLogError::SequenceExhausted,
    ] {
        let mut error = Some(error);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (org, _, task) = spawn(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err(error.take().expect("no retries")))
        });
        assert!(matches!(
            org.execute(project(), now()).await,
            Err(OrgServiceError::Unavailable(_))
        ));
        assert!(matches!(
            org.execute(project(), now()).await,
            Err(OrgServiceError::Unavailable(_))
        ));
        assert!(org.snapshot().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn before_handover_timeout_keeps_state_known_and_never_auto_retries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (org, cancel, task) = spawn(move |_| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(if n == 0 {
            Err(EventLogError::TimeoutBeforeHandover)
        } else {
            Ok(1)
        })
    });
    assert!(matches!(
        org.execute(project(), now()).await,
        Err(OrgServiceError::Timeout)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(org.status().unwrap(), 0);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 0);
    org.execute(project(), now()).await.unwrap();
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn failed_second_event_preserves_committed_prefix() {
    let mut state = OrgState::default();
    let id = "demo/lead".parse().unwrap();
    state
        .apply(
            1,
            now(),
            &OrgEvent::ProjectRegistered {
                project_id: "demo".parse().unwrap(),
                root: "/demo".into(),
            },
        )
        .unwrap();
    state
        .apply(
            2,
            now(),
            &OrgEvent::AgentAdopted {
                agent_id: id,
                pane: "%1".into(),
                session_id: None,
            },
        )
        .unwrap();
    let command = OrgCommand::Respawn {
        agent_id: "demo/lead".parse().unwrap(),
        role: atm_org::parse_role("lead", "+++\nharness='pi'\n+++\nInstructions").unwrap(),
        reason: "restart".into(),
    };
    let (health, status) = watch::channel(Ok(2));
    let mut kinds = Vec::new();
    let result = execute(
        &mut state,
        &command,
        now(),
        None,
        &health,
        &broadcast::channel(1024).0,
        &mut |envelope: Envelope| {
            kinds.push(envelope.kind);
            std::future::ready(if kinds.len() == 1 {
                Ok(3)
            } else {
                Err(EventLogError::TimeoutAfterHandover)
            })
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(kinds, ["agent.down", "agent.up_requested"]);
    assert_eq!(
        state.agent(&"demo/lead".parse().unwrap()).unwrap().status,
        AgentStatus::Down
    );
    assert!(status.borrow().is_err());
}

// Break caught: acceptance is acknowledged before its cost append, or an
// uncertain cost append is silently retried/hidden instead of preserving prefix.
#[tokio::test]
async fn failed_accepted_cost_append_preserves_verdict_prefix() {
    let mut state = OrgState::default();
    for line in include_str!("../../../atm-org/tests/fixtures/task_cost_log.jsonl")
        .lines()
        .take(7)
    {
        let envelope: Envelope = serde_json::from_str(line).unwrap();
        let event = OrgEvent::from_parts(&envelope.kind, &envelope.body)
            .unwrap()
            .unwrap();
        state.apply(envelope.seq, envelope.ts, &event).unwrap();
    }
    let command = OrgCommand::Task(atm_org::TaskCommand::Verdict {
        task_id: "t3".parse().unwrap(),
        verdict: atm_org::TaskVerdict::Accepted,
        review_task: None,
        note: None,
        by: atm_org::TaskActor {
            who: atm_org::Address::Owner,
            manager_of: None,
        },
    });
    let (health, status) = watch::channel(Ok(7));
    let mut kinds = Vec::new();
    let result = execute(
        &mut state,
        &command,
        now(),
        None,
        &health,
        &broadcast::channel(8).0,
        &mut |e: Envelope| {
            kinds.push(e.kind);
            std::future::ready(if kinds.len() == 1 {
                Ok(8)
            } else {
                Err(EventLogError::TimeoutAfterHandover)
            })
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(kinds, ["task.verdict", "task.cost_recorded"]);
    let task = state.task(&"t3".parse().unwrap()).unwrap();
    assert_eq!(task.state, atm_org::TaskState::Accepted);
    assert!(task.cost.is_none());
    assert!(status.borrow().is_err());
}

// Characterizes crrv-B L4: a known-unwritten second append does not roll back
// the verdict, retry its cost, or finish the review. Only startup repairs cost.
#[tokio::test]
async fn second_append_timeout_leaves_verdict_without_cost_or_review_closure() {
    reviewed_verdict_timeout_prefix(2).await;
}

// The third append is the review verdict, after the judged task's cost record.
// A retry/startup must not be mistaken for recovery of that missing review close.
#[tokio::test]
async fn third_append_timeout_leaves_recorded_cost_without_review_closure() {
    reviewed_verdict_timeout_prefix(3).await;
}

async fn reviewed_verdict_timeout_prefix(failed_append: usize) {
    use crate::eventlog::{read_from, Stream};
    use atm_org::{Address, TaskActor, TaskCommand, TaskState, TaskSubmission, TaskVerdict};

    let dir = tempfile::tempdir().unwrap();
    let segment = dir.path().join("log/events/00000001.jsonl");
    std::fs::create_dir_all(segment.parent().unwrap()).unwrap();
    let prefix = include_str!("../../../atm-org/tests/fixtures/task_cost_log.jsonl")
        .lines()
        .take(7)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&segment, prefix).unwrap();
    let ready = crate::org::storage::open(dir.path().into()).await.unwrap();
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = watch::channel(Ok(7));
    let (committed, _) = broadcast::channel(1024);
    let cancel = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (attempted, mut attempts) = mpsc::unbounded_channel();
    let actor = tokio::spawn(run(
        ready.state,
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        move |envelope| {
            let call = count.fetch_add(1, Ordering::SeqCst) + 1;
            attempted.send(envelope.clone()).unwrap();
            let disk = ready.log.clone();
            async move {
                if call == 5 + failed_append {
                    Err(EventLogError::TimeoutBeforeHandover)
                } else {
                    disk.append(envelope.ts, &envelope.kind, envelope.body)
                        .await
                }
            }
        },
    ));
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: None,
    };
    let owner = TaskActor {
        who: Address::Owner,
        manager_of: None,
    };
    let work: atm_org::TaskId = "t3".parse().unwrap();
    let review: atm_org::TaskId = "t9".parse().unwrap();
    let at: DateTime<Utc> = "2026-01-01T00:00:04Z".parse().unwrap();
    for command in [
        OrgCommand::Adopt {
            agent_id: "p/review".parse().unwrap(),
            pane: "%2".into(),
            session_id: None,
        },
        OrgCommand::Task(TaskCommand::Create(
            serde_json::from_value(serde_json::json!({
                "title": "review", "kind": "review", "creator": "owner",
                "judges": "t3", "measure": {"kind": "owner"}, "files": []
            }))
            .unwrap(),
        )),
        OrgCommand::Task(TaskCommand::Assign {
            task_id: review.clone(),
            agent_id: "p/review".parse().unwrap(),
            by: owner.clone(),
        }),
        OrgCommand::Task(TaskCommand::StartReview {
            task_id: work.clone(),
            review_task: review.clone(),
            by: owner.clone(),
        }),
        OrgCommand::Task(TaskCommand::Submit {
            task_id: review.clone(),
            what: TaskSubmission::Result,
            files: vec![],
            by: TaskActor {
                who: "p/review".parse().unwrap(),
                manager_of: None,
            },
        }),
    ] {
        org.execute(command, at).await.unwrap();
        attempts.try_recv().unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(org.status().unwrap(), 12);
    let mut published = org.committed.subscribe();
    let accepted_at = at + chrono::Duration::seconds(1);
    let command = TaskCommand::Verdict {
        task_id: work.clone(),
        verdict: TaskVerdict::Accepted,
        review_task: Some(review.clone()),
        note: Some("accepted after review".into()),
        by: owner,
    };
    assert!(matches!(
        org.execute_task(command.clone(), accepted_at).await,
        Err(OrgServiceError::Timeout)
    ));
    let wanted = [
        ("task.verdict", "t3"),
        ("task.cost_recorded", "t3"),
        ("task.verdict", "t9"),
    ];
    for (kind, task_id) in wanted.into_iter().take(failed_append) {
        let attempt = attempts.try_recv().unwrap();
        assert_eq!(attempt.kind, kind);
        assert_eq!(attempt.body["task_id"], task_id);
    }
    assert!(matches!(
        attempts.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let fence = 12 + failed_append as u64 - 1;
    assert_eq!(org.status().unwrap(), fence, "known prefix remains healthy");
    let prefix = org.snapshot().await.unwrap();
    let judged = prefix.task(&work).unwrap();
    assert_eq!(judged.state, TaskState::Accepted);
    assert_eq!(judged.changed_at, accepted_at);
    assert_eq!(judged.cost.is_some(), failed_append == 3);
    assert!(
        judged.review_task.is_none(),
        "acceptance clears the live review link"
    );
    assert_eq!(judged.reason.as_deref(), Some("accepted after review"));
    assert_eq!(prefix.task(&review).unwrap().state, TaskState::Submitted);
    assert!(prefix.task(&review).unwrap().cost.is_none());
    let durable: Vec<_> = read_from(dir.path(), Stream::Events, 12)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(durable.len(), failed_append - 1);
    for envelope in &durable {
        assert_eq!(published.try_recv().unwrap().as_ref(), envelope);
    }
    assert!(matches!(
        published.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    let retry = org
        .execute_task(command.clone(), accepted_at)
        .await
        .unwrap();
    assert!(
        retry.events.is_empty(),
        "a verdict retry does not resume its suffix"
    );
    assert_eq!(retry.before.as_ref(), prefix.task(&work));
    assert_eq!(calls.load(Ordering::SeqCst), 5 + failed_append);
    assert_eq!(org.snapshot().await.unwrap(), prefix);
    assert_eq!(org.status().unwrap(), fence);
    assert!(matches!(
        published.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    cancel.cancel();
    actor.await.unwrap();

    // Replay a copy of the acknowledged disk prefix: no flock-release polling,
    // and no hand-built repaired state. The real startup actor must repair it.
    let restart = tempfile::tempdir().unwrap();
    let restarted_segment = restart.path().join("log/events/00000001.jsonl");
    std::fs::create_dir_all(restarted_segment.parent().unwrap()).unwrap();
    std::fs::copy(&segment, &restarted_segment).unwrap();
    let stop = CancellationToken::new();
    let reopened = OrgHandle::open(restart.path().into(), stop.clone()).await;
    let repaired = reopened.snapshot().await.unwrap();
    assert_eq!(reopened.status().unwrap(), 14);
    let record = repaired.task(&work).unwrap().cost.as_ref().unwrap();
    assert_eq!(
        record.cost,
        Some(atm_core::Money::from_microdollars(25_000))
    );
    assert_eq!(record.turns, Some(1));
    assert_eq!(record.review_rounds, Some(1));
    assert_eq!(record.seconds_in_state.values().sum::<u64>(), 5);
    let mut expected = prefix;
    // All fields other than the missing cost must still come from replay.
    let repaired_records: Vec<_> = read_from(restart.path(), Stream::Events, 12)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(repaired_records.len(), 2);
    let cost = repaired_records.last().unwrap();
    assert_eq!(cost.kind, "task.cost_recorded");
    assert_eq!(cost.seq, 14);
    assert_eq!(
        cost.ts, accepted_at,
        "repair retains the verdict's timestamp"
    );
    assert_eq!(cost.body["task_id"], "t3");
    if failed_append == 2 {
        expected
            .apply(
                14,
                cost.ts,
                &OrgEvent::from_parts(&cost.kind, &cost.body)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
    } else {
        assert_eq!(repaired_records, durable);
    }
    assert_eq!(
        repaired, expected,
        "startup repairs cost, not the missing review verdict"
    );
    assert!(reopened
        .execute_task(command, accepted_at)
        .await
        .unwrap()
        .events
        .is_empty());
    assert_eq!(reopened.status().unwrap(), 14);
    assert_eq!(reopened.snapshot().await.unwrap(), repaired);
    stop.cancel();
}

// Break caught: publication or command acknowledgement crosses a still-pending
// cost append. Only the disk boundary is stalled; decide/apply/publication run.
#[tokio::test]
async fn accepted_cost_waits_for_its_append_before_publication_and_reply() {
    let mut state = OrgState::default();
    for line in include_str!("../../../atm-org/tests/fixtures/task_cost_log.jsonl")
        .lines()
        .take(7)
    {
        let envelope: Envelope = serde_json::from_str(line).unwrap();
        state
            .apply(
                envelope.seq,
                envelope.ts,
                &OrgEvent::from_parts(&envelope.kind, &envelope.body)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
    }
    let command = OrgCommand::Task(atm_org::TaskCommand::Verdict {
        task_id: "t3".parse().unwrap(),
        verdict: atm_org::TaskVerdict::Accepted,
        review_task: None,
        note: None,
        by: atm_org::TaskActor {
            who: atm_org::Address::Owner,
            manager_of: None,
        },
    });
    let (health, status) = watch::channel(Ok(7));
    let (publication, mut published) = broadcast::channel(8);
    let (entered, mut entry) = mpsc::channel(1);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut seq = 7;
    let mut append = |e: Envelope| {
        seq += 1;
        let seq = seq;
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            if e.kind == "task.cost_recorded" {
                entered.send(()).await.unwrap();
                let _permit = gate.acquire().await.unwrap();
            }
            Ok(seq)
        }
    };
    let work = execute(
        &mut state,
        &command,
        now(),
        None,
        &health,
        &publication,
        &mut append,
    );
    tokio::pin!(work);
    tokio::select! { result = &mut work => panic!("accepted command returned before cost append: {result:?}"), _ = entry.recv() => {} }
    assert_eq!(*status.borrow(), Ok(8));
    assert_eq!(published.try_recv().unwrap().kind, "task.verdict");
    assert!(matches!(
        published.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    gate.add_permits(1);
    assert_eq!(work.await.unwrap().len(), 2);
    assert_eq!(*status.borrow(), Ok(9));
    assert_eq!(published.try_recv().unwrap().kind, "task.cost_recorded");
}

#[tokio::test(start_paused = true)]
async fn caller_timeout_does_not_cancel_a_later_commit() {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let (org, cancel, task) = spawn(move |_| {
        let gate = gate.clone();
        async move {
            let _permit = gate.acquire().await.unwrap();
            Ok(1)
        }
    });
    assert!(matches!(
        org.execute(project(), now()).await,
        Err(OrgServiceError::Timeout)
    ));
    assert_eq!(org.status().unwrap(), 0);
    release.add_permits(1);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn queue_overload_is_an_explicit_refusal_not_unbounded_work() {
    let (requests, _receive) = mpsc::channel(1);
    let (_health, status) = watch::channel(Ok(0));
    let org = OrgHandle {
        requests,
        health: status,
        committed: broadcast::channel(1024).0,
        reads: None,
    };
    let (reply, _) = oneshot::channel();
    org.submit(Request::Snapshot(reply)).unwrap();
    let (reply, _) = oneshot::channel();
    let error = org.submit(Request::Snapshot(reply)).unwrap_err();
    assert!(error.to_string().contains("queue"));
    assert_eq!(org.status().unwrap(), 0);
}
