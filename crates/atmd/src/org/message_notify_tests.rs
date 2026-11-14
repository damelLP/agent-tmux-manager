use super::*;
use crate::{control::reservation::PaneId, org::messages::focus_notice};
use atm_org::{MessageContent, MessageKind, OrgCommand};
use chrono::{DateTime, Utc};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Semaphore;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}
fn content() -> MessageContent {
    MessageContent {
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
    assert_eq!(first.id, MessageId::new(7));
    assert!(first.message.contains(&inbox_reference(first.id)));
    assert!(!first.message.contains("private body"));
}
#[tokio::test]
async fn focus_notice_never_notifies_but_user_lookalike_does() {
    let (tx, mut rx) = mpsc::channel(32);
    let mut cursor = 0;
    let focus = focus_notice(MessageId::new(1), &PaneId::parse("%7").unwrap());
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
    let mut user = focus;
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
    assert_eq!(rx.recv().await.unwrap().id, MessageId::new(2));
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
    assert_eq!(rx.recv().await.unwrap().id.seq(), committed[0].seq);
    org.execute(OrgCommand::SendMessage(content()), now())
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap().id.seq(), committed[0].seq + 1);
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
        assert_eq!(rx.recv().await.unwrap().id.seq(), seq);
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
    let actor = tokio::spawn(actor::run(
        atm_org::OrgState::default(),
        receive,
        health,
        committed.clone(),
        cancel.clone(),
        |_| async {
            Err(crate::eventlog::EventLogError::Io {
                path: "events".into(),
                source: std::io::Error::other("append failed"),
            })
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
    assert!(org
        .execute(OrgCommand::SendMessage(content()), now())
        .await
        .is_err());
    cancel.cancel();
    actor.await.unwrap();
    consumer.await.unwrap();
    assert!(rx.recv().await.is_none());
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
    assert!(message.contains(&inbox_reference(MessageId::new(committed[0].seq))));
    assert!(!message.contains("private body"));
    assert_eq!(
        org.snapshot().await.unwrap().messages().count(),
        1,
        "notification never creates another message"
    );
    cancel.cancel();
    task.await.unwrap();
}
