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
