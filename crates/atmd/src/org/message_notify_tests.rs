use super::*;
use crate::{control::reservation::PaneId, org::messages::focus_notice};
use atm_org::{MessageContent, MessageKind, OrgCommand};
use chrono::{DateTime, Utc};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Semaphore;

fn message_id(subject: Subject) -> MessageId {
    let Subject::Message(id) = subject else {
        panic!("expected message notice");
    };
    id
}

fn decision_record(seq: u64) -> Envelope {
    let request = atm_org::DecisionRequest {
        requester: "p/worker".parse().unwrap(),
        kind: atm_org::DecisionKind::Plan,
        title: "private title".into(),
        context: "private context".into(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: Some("yes".into()),
        may_be_undone: true,
        changes_intent: true,
        deadline: None,
    };
    Envelope {
        seq,
        ts: now(),
        kind: "decision.requested".into(),
        body: serde_json::to_value(request).unwrap(),
    }
}

#[tokio::test]
async fn committed_decision_requests_queue_a_reference_not_private_context() {
    let (tx, mut rx) = mpsc::channel(32);
    assert_eq!(
        enqueue(
            &decision_record(41),
            &mut 0,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Queued
    );
    let notice = rx.recv().await.unwrap();
    assert_eq!(notice.id.to_string(), "d41");
    assert_eq!(notice.message, "d41 from p/worker: atm decisions");
}

#[tokio::test(start_paused = true)]
async fn decisions_no_longer_pending_suppress_delayed_notifications() {
    let mut state = OrgState::default();
    for (seq, command) in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
    ]
    .into_iter()
    .enumerate()
    {
        for event in state.decide(&command, now()).unwrap() {
            state.apply(seq as u64 + 1, now(), &event).unwrap();
        }
    }
    state
        .apply(
            3,
            now(),
            &OrgEvent::from_parts("decision.requested", &decision_record(3).body)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    assert!(still_open(&state, Subject::Decision(DecisionId::new(3))));
    let answer = atm_org::DecisionResolution {
        decision_id: DecisionId::new(3),
        option: "yes".into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    };
    for event in [
        OrgEvent::DecisionResolved(answer.clone().into()),
        OrgEvent::DecisionOverruled(answer.into()),
        OrgEvent::DecisionExpired {
            decision_id: DecisionId::new(3),
        },
        OrgEvent::DecisionWithdrawn(atm_org::DecisionWithdrawal {
            decision_id: DecisionId::new(3),
            by: Address::Owner,
            reason: None,
        }),
        OrgEvent::DecisionTakenProvisionally(atm_org::ProvisionalChoice {
            relayed_by: None,
            decision_id: DecisionId::new(3),
            option: "yes".into(),
            by: Address::System,
        }),
    ] {
        let mut closed = state.clone();
        closed.apply(4, now(), &event).unwrap();
        let (tx, rx) = mpsc::channel(32);
        enqueue(
            &decision_record(3),
            &mut 0,
            &tx,
            Duration::from_secs(5),
            Instant::now(),
        )
        .unwrap();
        drop(tx);
        let launches = AtomicUsize::new(0);
        run_queue(
            rx,
            CancellationToken::new(),
            |subject| std::future::ready(Ok(still_open(&closed, subject))),
            |_| {
                launches.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(()))
            },
        )
        .await;
        assert_eq!(launches.load(Ordering::SeqCst), 0, "{event:?}");
    }
}

#[tokio::test]
async fn committed_decision_reaches_existing_runner_as_one_literal_argument() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
    ] {
        org.execute(command, now()).await.unwrap();
    }
    let socket = dir.path().join("notify.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let config = NotifyConfig { command: vec!["python3".into(), "-c".into(), "import json,socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(json.dumps(sys.argv[2:]).encode()); s.close()".into(), socket.to_string_lossy().into_owned()], delay_secs: 0 };
    let task = spawn_message_notifier(org.clone(), config, cancel.clone()).unwrap();
    let mut request: atm_org::DecisionRequest =
        serde_json::from_value(decision_record(3).body).unwrap();
    request.requester = Address::Owner;
    assert!(org
        .execute(
            OrgCommand::Decision(atm_org::DecisionCommand::Request(request.clone())),
            now()
        )
        .await
        .is_err());
    assert_eq!(org.status().unwrap(), 2, "a refused request emits nothing");
    request.requester = "p/worker".parse().unwrap();
    org.execute(
        OrgCommand::Decision(atm_org::DecisionCommand::Request(request)),
        now(),
    )
    .await
    .unwrap();
    use tokio::io::AsyncReadExt;
    let argv: serde_json::Value = tokio::time::timeout(Duration::from_secs(60), async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(argv, serde_json::json!(["d3 from p/worker: atm decisions"]));
    assert_eq!(
        org.snapshot().await.unwrap().messages().count(),
        0,
        "a decision notice never invents an inbox message"
    );
    cancel.cancel();
    task.await.unwrap();
}

// Break caught: watching from sequence zero on startup re-notifies historical
// pending decisions and unread owner messages before the new decision notice.
#[tokio::test]
async fn notifier_starts_at_current_cursor_without_repeating_pending_history() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
    for command in [
        OrgCommand::RegisterProject {
            project_id: "p".parse().unwrap(),
            root: "/project".into(),
            move_root: false,
        },
        OrgCommand::Adopt {
            agent_id: "p/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: None,
        },
    ] {
        org.execute(command, now()).await.unwrap();
    }
    let request: atm_org::DecisionRequest =
        serde_json::from_value(decision_record(3).body).unwrap();
    org.execute(
        OrgCommand::Decision(atm_org::DecisionCommand::Request(request.clone())),
        now(),
    )
    .await
    .unwrap();
    org.execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    assert_eq!(org.status().unwrap(), 4);
    let socket = dir.path().join("notify.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let config = NotifyConfig {
        command: vec!["python3".into(), "-c".into(),
            "import json,socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(json.dumps(sys.argv[2:]).encode()); s.close()".into(),
            socket.to_string_lossy().into_owned()],
        delay_secs: 0,
    };
    let task = spawn_message_notifier(org.clone(), config, cancel.clone()).unwrap();
    org.execute(
        OrgCommand::Decision(atm_org::DecisionCommand::Request(request)),
        now(),
    )
    .await
    .unwrap();
    use tokio::io::AsyncReadExt;
    let first: serde_json::Value = tokio::time::timeout(Duration::from_secs(60), async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        first,
        serde_json::json!(["d5 from p/worker: atm decisions"])
    );
    let state = org.snapshot().await.unwrap();
    assert!(still_open(&state, Subject::Decision(DecisionId::new(3))));
    assert!(still_open(&state, Subject::Message(MessageId::new(4))));
    cancel.cancel();
    task.await.unwrap();
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}
fn content() -> MessageContent {
    MessageContent {
        delivery_notice: None,
        from: Address::Owner,
        to: Address::Owner,
        kind: MessageKind::Note,
        body: "private body".into(),
        attachments: vec![],
        task: None,
    }
}
fn envelope(seq: u64, c: MessageContent) -> Envelope {
    Envelope {
        seq,
        ts: now(),
        kind: "msg.sent".into(),
        body: OrgEvent::MessageSent(c).to_body(),
    }
}
#[tokio::test]
async fn only_new_owner_commits_queue_and_overflow_is_observable() {
    let (tx, mut rx) = mpsc::channel(32);
    let mut cursor = 5;
    assert_eq!(
        enqueue(
            &envelope(5, content()),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Ignored
    );
    let mut agent = content();
    agent.to = "p/a".parse().unwrap();
    assert_eq!(
        enqueue(
            &envelope(6, agent),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Ignored
    );
    for seq in 7..39 {
        assert_eq!(
            enqueue(
                &envelope(seq, content()),
                &mut cursor,
                &tx,
                Duration::ZERO,
                Instant::now()
            )
            .unwrap(),
            Admission::Queued
        );
    }
    assert_eq!(
        enqueue(
            &envelope(39, content()),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Overflow
    );
    assert_eq!(cursor, 39);
    assert_eq!(
        enqueue(
            &envelope(7, content()),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Ignored
    );
    let first = rx.recv().await.unwrap();
    assert_eq!(message_id(first.id), MessageId::new(7));
    assert!(first.message.contains(&format!(
        "{} --for owner",
        inbox_reference(message_id(first.id))
    )));
    assert!(!first.message.contains("private body"));
}
#[tokio::test]
async fn focus_notice_never_notifies_but_user_lookalike_does() {
    let (tx, mut rx) = mpsc::channel(32);
    let mut cursor = 0;
    let mut focus = focus_notice(MessageId::new(1), &PaneId::parse("%7").unwrap());
    focus.delivery_notice = Some(atm_org::DeliveryNotice {
        source_message_id: MessageId::new(1),
        reason: "focus".into(),
    });
    assert_eq!(
        enqueue(
            &envelope(1, focus.clone()),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Ignored
    );
    let mut legacy = focus.clone();
    legacy.delivery_notice = None;
    assert!(is_focus_notice(&legacy));
    for (id, reason) in [(2, "focus"), (1, "unobserved")] {
        let mut wrong = focus.clone();
        wrong.delivery_notice = Some(atm_org::DeliveryNotice {
            source_message_id: MessageId::new(id),
            reason: reason.into(),
        });
        assert!(!is_focus_notice(&wrong));
    }
    let mut user = focus;
    user.delivery_notice = None;
    user.from = Address::Owner;
    assert_eq!(
        enqueue(
            &envelope(2, user),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Queued
    );
    assert_eq!(message_id(rx.recv().await.unwrap().id), MessageId::new(2));
}
#[tokio::test(start_paused = true)]
async fn read_before_due_and_read_while_queued_suppress_at_launch() {
    let start = Instant::now();
    let (tx, rx) = mpsc::channel(32);
    let mut cursor = 0;
    let unread = Arc::new(AtomicUsize::new(1));
    enqueue(
        &envelope(1, content()),
        &mut cursor,
        &tx,
        Duration::from_secs(10),
        Instant::now(),
    )
    .unwrap();
    enqueue(
        &envelope(2, content()),
        &mut cursor,
        &tx,
        Duration::from_secs(10),
        Instant::now(),
    )
    .unwrap();
    drop(tx);
    let observed = unread.clone();
    let launches = Arc::new(AtomicUsize::new(0));
    let seen = launches.clone();
    unread.store(0, Ordering::SeqCst);
    run_queue(
        rx,
        CancellationToken::new(),
        move |_| {
            let unread = observed.load(Ordering::SeqCst) > 0;
            async move { Ok(unread) }
        },
        move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        },
    )
    .await;
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert_eq!(Instant::now() - start, Duration::from_secs(10));
}
#[tokio::test]
async fn stalled_command_keeps_ingestion_bounded_and_only_one_command_runs() {
    let (tx, rx) = mpsc::channel(32);
    let mut cursor = 0;
    enqueue(
        &envelope(1, content()),
        &mut cursor,
        &tx,
        Duration::ZERO,
        Instant::now(),
    )
    .unwrap();
    let (started, mut starts) = mpsc::channel(2);
    let gate = Arc::new(Semaphore::new(0));
    let release = gate.clone();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_queue(
        rx,
        cancel.clone(),
        |_| async { Ok(true) },
        move |message| {
            let started = started.clone();
            let gate = gate.clone();
            async move {
                started.send(message).await.unwrap();
                gate.acquire().await.unwrap().forget();
                Ok(())
            }
        },
    ));
    starts.recv().await.unwrap();
    for seq in 2..34 {
        assert_eq!(
            enqueue(
                &envelope(seq, content()),
                &mut cursor,
                &tx,
                Duration::ZERO,
                Instant::now()
            )
            .unwrap(),
            Admission::Queued
        );
    }
    assert_eq!(
        enqueue(
            &envelope(34, content()),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now()
        )
        .unwrap(),
        Admission::Overflow
    );
    assert!(starts.try_recv().is_err());
    release.add_permits(1);
    starts.recv().await.unwrap();
    cancel.cancel();
    task.await.unwrap();
    assert!(starts.recv().await.is_none());
}
#[tokio::test(start_paused = true)]
async fn unavailable_snapshot_suppresses_and_cancel_drops_delayed_work() {
    for unavailable in [false, true] {
        let (tx, rx) = mpsc::channel(32);
        let mut cursor = 0;
        enqueue(
            &envelope(1, content()),
            &mut cursor,
            &tx,
            Duration::from_secs(10),
            Instant::now(),
        )
        .unwrap();
        drop(tx);
        let cancel = CancellationToken::new();
        if !unavailable {
            cancel.cancel();
        }
        let launches = Arc::new(AtomicUsize::new(0));
        let seen = launches.clone();
        run_queue(
            rx,
            cancel,
            |_| async { Err(OrgServiceError::Unavailable("lost".into())) },
            move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(launches.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn watch_startup_cursor_and_overlap_only_notify_new_commits() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    org.execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    let cursor = org.status().unwrap();
    let committed = org
        .execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    let watch = org.watch(vec!["msg.sent".into()], cursor).unwrap();
    let (tx, mut rx) = mpsc::channel(32);
    let done = cancel.clone();
    let task = tokio::spawn(consume(watch, cursor, tx, Duration::ZERO, done));
    assert_eq!(
        message_id(rx.recv().await.unwrap().id).seq(),
        committed[0].seq
    );
    org.execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    assert_eq!(
        message_id(rx.recv().await.unwrap().id).seq(),
        committed[0].seq + 1
    );
    cancel.cancel();
    task.await.unwrap();
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn watch_lag_replays_commits_once() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let watch = org.watch(vec!["msg.sent".into()], 0).unwrap();
    for _ in 0..1030 {
        org.execute(OrgCommand::SendMessage(content()), now())
            .await
            .unwrap();
    }
    // Large test receiver isolates watch replay from the independently tested
    // production 32-entry admission limit.
    let (tx, mut rx) = mpsc::channel(2048);
    let task = tokio::spawn(consume(watch, 0, tx, Duration::ZERO, cancel.clone()));
    for seq in 1..=1030 {
        assert_eq!(message_id(rx.recv().await.unwrap().id).seq(), seq);
    }
    cancel.cancel();
    task.await.unwrap();
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn read_while_command_is_stalled_suppresses_queued_notice() {
    let (tx, rx) = mpsc::channel(32);
    let mut cursor = 0;
    for seq in 1..=2 {
        enqueue(
            &envelope(seq, content()),
            &mut cursor,
            &tx,
            Duration::ZERO,
            Instant::now(),
        )
        .unwrap();
    }
    drop(tx);
    let unread = Arc::new(AtomicUsize::new(1));
    let state = unread.clone();
    let (started, mut starts) = mpsc::channel(2);
    let gate = Arc::new(Semaphore::new(0));
    let release = gate.clone();
    let task = tokio::spawn(run_queue(
        rx,
        CancellationToken::new(),
        move |_| {
            let value = state.load(Ordering::SeqCst) > 0;
            async move { Ok(value) }
        },
        move |text| {
            let started = started.clone();
            let gate = gate.clone();
            async move {
                started.send(text).await.unwrap();
                gate.acquire().await.unwrap().forget();
                Ok(())
            }
        },
    ));
    starts.recv().await.unwrap();
    unread.store(0, Ordering::SeqCst);
    release.add_permits(1);
    task.await.unwrap();
    assert!(starts.recv().await.is_none());
}

#[tokio::test]
async fn failed_append_has_no_notification() {
    use crate::org::{actor, storage, watch, OrgHandle};
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let (requests, receive) = mpsc::channel(64);
    let (health, status) = tokio::sync::watch::channel(Ok(0));
    let (committed, _) = tokio::sync::broadcast::channel(16);
    let mut fail = true;
    let actor = tokio::spawn(actor::run(
        atm_org::OrgState::default(),
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        move |_| {
            let failed = std::mem::take(&mut fail);
            async move {
                if failed {
                    // Unlike fatal I/O errors, pre-handover timeout allows another command.
                    Err(crate::eventlog::EventLogError::TimeoutBeforeHandover)
                } else {
                    Ok(1)
                }
            }
        },
    ));
    let org = OrgHandle {
        requests,
        health: status,
        committed,
        reads: Some(watch::ReadAccess {
            reader: storage::Reader::start(dir.path().into()).unwrap(),
        }),
    };
    let watch = org.watch(vec!["msg.sent".into()], 0).unwrap();
    let (tx, mut rx) = mpsc::channel(32);
    let consumer = tokio::spawn(consume(watch, 0, tx, Duration::ZERO, cancel.clone()));
    assert!(matches!(
        org.execute(OrgCommand::SendMessage(content()), now()).await,
        Err(OrgServiceError::Timeout)
    ));
    let mut successful = content();
    successful.from = Address::System;
    let committed = org
        .execute(OrgCommand::SendMessage(successful), now())
        .await
        .unwrap();
    // This successful commit fences the failed append; cancellation must not mask it.
    let notice = tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message_id(notice.id).seq(), committed[0].seq);
    assert!(
        notice.message.contains("from system:"),
        "{}",
        notice.message
    );
    cancel.cancel();
    actor.await.unwrap();
    consumer.await.unwrap();
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn notification_failure_does_not_retry_or_generate_another_message() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let committed = org
        .execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    let cursor = org.status().unwrap();
    let (tx, rx) = mpsc::channel(32);
    enqueue(&committed[0], &mut 0, &tx, Duration::ZERO, Instant::now()).unwrap();
    drop(tx);
    let launches = AtomicUsize::new(0);
    tokio::time::timeout(
        Duration::from_secs(30),
        run_queue(
            rx,
            cancel.clone(),
            |id| {
                let org = org.clone();
                async move {
                    Ok(org
                        .snapshot()
                        .await?
                        .message(&message_id(id))
                        .is_some_and(|m| m.read_at.is_none()))
                }
            },
            |_| {
                launches.fetch_add(1, Ordering::SeqCst);
                async {
                    let result = run_notify(
                        &["python3".into(), "-c".into(), "raise SystemExit(7)".into()],
                        NOTIFY_TIMEOUT,
                    )
                    .await;
                    assert!(matches!(result, Err(NotifyError::Failed { .. })));
                    result
                }
            },
        ),
    )
    .await
    .unwrap();
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(
        org.status().unwrap(),
        cursor,
        "notification failure must not write events"
    );
    assert_eq!(org.snapshot().await.unwrap().messages().count(), 1);
    cancel.cancel();
}

#[tokio::test]
async fn committed_message_reaches_existing_runner_as_one_literal_argument() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    // The configured Python program acknowledges argv through a private Unix
    // socket; the production runner adds no shell and the test needs no waits.
    let socket = dir.path().join("notify.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let code="import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode()); s.close()";
    let config = NotifyConfig {
        command: vec![
            "python3".into(),
            "-c".into(),
            code.into(),
            socket.to_string_lossy().into_owned(),
        ],
        delay_secs: 0,
    };
    let task = spawn_message_notifier(org.clone(), config, cancel.clone()).unwrap();
    let committed = org
        .execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    use tokio::io::AsyncReadExt;
    let message = tokio::time::timeout(Duration::from_secs(30), async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).await.unwrap();
        text
    })
    .await
    .unwrap();
    assert!(
        message.contains(&format!(
            "{} --for owner",
            inbox_reference(MessageId::new(committed[0].seq))
        )),
        "{message}"
    );
    assert!(!message.contains("private body"));
    assert_eq!(
        org.snapshot().await.unwrap().messages().count(),
        1,
        "notification never creates another message"
    );
    cancel.cancel();
    task.await.unwrap();
}
