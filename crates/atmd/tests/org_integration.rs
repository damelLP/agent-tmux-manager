//! Durable organisation service: real files, isolated state, no harness or tmux.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_core::{Harness, Money, SessionId};
use atm_org::{OrgCommand, OrgEvent, SessionSummary};
use atm_protocol::{ClientMessage, DaemonMessage};
use atmd::eventlog::{read_from, EventLog, Stream};
use atmd::org::{OrgHandle, RawHandle, SummaryObserver};
use atmd::server::{methods::MethodServices, DaemonServer};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{
    unix::{OwnedReadHalf, OwnedWriteHalf},
    UnixStream,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct SocketServer {
    _dir: tempfile::TempDir,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
}
impl SocketServer {
    async fn start(services: MethodServices) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atm.sock");
        let cancel = CancellationToken::new();
        let server = DaemonServer::with_services(&path, services, cancel.clone());
        let task = tokio::spawn(async move { server.run().await });
        let stream = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = UnixStream::connect(&path).await {
                    break stream;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let (read, write) = stream.into_split();
        let mut server = Self {
            _dir: dir,
            cancel,
            task,
            read: BufReader::new(read),
            write,
        };
        server.send(ClientMessage::connect(None)).await;
        assert!(matches!(
            server.recv().await,
            DaemonMessage::Connected { .. }
        ));
        server
    }
    async fn send(&mut self, frame: ClientMessage) {
        timeout(
            Duration::from_secs(5),
            self.write
                .write_all(format!("{}\n", serde_json::to_string(&frame).unwrap()).as_bytes()),
        )
        .await
        .unwrap()
        .unwrap();
    }
    async fn recv(&mut self) -> DaemonMessage {
        let mut line = String::new();
        timeout(Duration::from_secs(5), self.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn barrier(&mut self) {
        self.send(ClientMessage::ping(987)).await;
        loop {
            if matches!(self.recv().await, DaemonMessage::Pong { seq: 987 }) {
                break;
            }
        }
    }
    async fn stop(self) {
        self.cancel.cancel();
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn all_adapter_envelopes_are_captured_before_parse_and_pi_context_is_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let raw = RawHandle::open(dir.path().to_owned()).await;
    let mut services = MethodServices::new(atmd::registry::spawn_registry());
    services.raw = raw.clone();
    let mut server = SocketServer::start(services).await;
    for frame in [
        ClientMessage::status_update(json!({"bad":1})),
        ClientMessage::hook_event(json!({"bad":2})),
        ClientMessage::pi_event(json!({"bad":3})),
        ClientMessage::codex_event(json!({"bad":4})),
    ] {
        server.send(frame).await;
    }
    // Synthetic routing probe: malformed context still has its conversation removed.
    server
        .send(ClientMessage::pi_event(
            json!({"event":"context","payload":{"messages":["private"]},"usage":{"cost":0.25}}),
        ))
        .await;
    server.barrier().await;
    raw.flush().await.unwrap();
    let records: Vec<_> = read_from(dir.path(), Stream::Raw, 0)
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        records.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
        [
            "status_update",
            "hook_event",
            "pi_event",
            "codex_event",
            "pi_event"
        ]
    );
    for (n, record) in records.iter().take(4).enumerate() {
        assert_eq!(record.body, json!({"bad":n+1}));
    }
    assert_eq!(
        records[4].body,
        json!({"event":"context","payload":{},"usage":{"cost":0.25}})
    );
    server.stop().await;
}

#[tokio::test]
async fn captured_cost_clean_flush_and_clear_end_snapshots() {
    for (fixture, finish) in [
        (
            include_str!("../../atm-pi-adapter/tests/fixtures/pi_session_cost_0.73.1.jsonl"),
            false,
        ),
        (
            include_str!("../../atm-pi-adapter/tests/fixtures/pi_new_session_0.73.1.jsonl"),
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let registry = atmd::registry::spawn_registry();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
        let observer = SummaryObserver::spawn(registry.clone(), org.clone());
        let pending = SessionId::pending_from_pid(std::process::id());
        registry
            .register_discovered(
                pending.clone(),
                std::process::id(),
                "/fixture".into(),
                Some("%0".into()),
                Harness::Pi,
            )
            .await
            .unwrap();
        let raw = RawHandle::open(dir.path().to_owned()).await;
        let mut services = MethodServices::new(registry.clone());
        services.org = org.clone();
        services.raw = raw.clone();
        let mut server = SocketServer::start(services).await;
        let mut ids = std::collections::HashSet::new();
        for line in fixture.lines() {
            let mut frame: serde_json::Value = serde_json::from_str(line).unwrap();
            let data = &mut frame["data"];
            if !finish && data["event"] == "session_shutdown" {
                continue;
            }
            data["pid"] = json!(std::process::id());
            if let Some(id) = data["session_id"].as_str() {
                ids.insert(id.to_owned());
            }
            server.send(serde_json::from_value(frame).unwrap()).await;
        }
        server.barrier().await;
        server.stop().await;
        observer.finish().await;
        raw.flush().await.unwrap();
        let state = org.snapshot().await.unwrap();
        assert!(
            state.session_summary(&pending).is_none(),
            "pending upgrade was double billed"
        );
        assert!(read_from(dir.path(), Stream::Events, 0)
            .map(Result::unwrap)
            .all(
                |record| record.body.get("session_id").and_then(|id| id.as_str())
                    != Some(pending.as_str())
            ));
        for id in ids {
            let summary = state.session_summary(&SessionId::new(id)).unwrap();
            if finish {
                assert!(summary.ended.is_some());
            } else {
                assert_eq!(summary.cost, Some(Money::from_microdollars(10650)));
                assert_eq!(
                    summary.first_prompt.as_deref(),
                    Some("Read note.txt and reply.")
                );
                assert!(summary.ended.is_none());
            }
        }
        cancel.cancel();
    }
}

#[tokio::test]
async fn wire_watch_unwatch_independent_of_unsubscribe_and_rewatch() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let registry = atmd::registry::spawn_registry();
    let mut services = MethodServices::new(registry.clone());
    services.org = org.clone();
    let mut server = SocketServer::start(services).await;
    org.execute(audit("one"), now()).await.unwrap();
    server
        .send(ClientMessage::watch(vec!["control".into()], 0))
        .await;
    let event = serde_json::to_value(server.recv().await).unwrap();
    assert_eq!(
        event,
        json!({"type":"event","topic":"control","seq":1,"ts":"2023-11-14T22:13:20Z","kind":"control.sent","body":{"pane":"%12","action":{"text":"one"}}})
    );
    server
        .send(ClientMessage::new(atm_protocol::MessageType::Unsubscribe))
        .await;
    server.send(ClientMessage::ping(7)).await;
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Pong { seq: 7 }
    ));
    org.execute(audit("two"), now()).await.unwrap();
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Event { seq: 2, .. }
    ));
    server.send(ClientMessage::subscribe(None)).await;
    assert!(matches!(
        server.recv().await,
        DaemonMessage::SessionList { .. }
    ));
    server.send(ClientMessage::unwatch()).await;
    server.send(ClientMessage::ping(8)).await;
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Pong { seq: 8 }
    ));
    org.execute(audit("three"), now()).await.unwrap();
    registry
        .register_discovered(
            SessionId::new("live"),
            1234,
            "/fixture".into(),
            None,
            Harness::Pi,
        )
        .await
        .unwrap();
    assert!(matches!(
        server.recv().await,
        DaemonMessage::SessionUpdated { .. }
    ));
    // Replacing an active watch must not leave the old forwarder alive.
    server
        .send(ClientMessage::watch(vec!["never".into()], 0))
        .await;
    server
        .send(ClientMessage::watch(vec!["control".into()], 2))
        .await;
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Event { seq: 3, .. }
    ));
    org.execute(audit("four"), now()).await.unwrap();
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Event { seq: 4, .. }
    ));
    server.send(ClientMessage::ping(9)).await;
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Pong { seq: 9 }
    ));
    server.stop().await;
    cancel.cancel();
}

#[tokio::test]
async fn wire_tail_oversized_envelope_and_id_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let mut services = MethodServices::new(atmd::registry::spawn_registry());
    services.org = org.clone();
    let mut server = SocketServer::start(services).await;
    org.execute(audit("x".repeat(300_000)), now())
        .await
        .unwrap();
    let id = "i".repeat(800_000);
    server
        .send(ClientMessage::request(
            id.clone(),
            "log.tail".into(),
            json!({}),
            None,
        ))
        .await;
    let result = serde_json::to_value(server.recv().await).unwrap();
    assert_eq!(result["id"], id);
    assert_eq!(result["error"]["code"], "refused");
    assert!(serde_json::to_vec(&result).unwrap().len() < 1_048_576);
    org.execute(audit("x".repeat(600_000)), now())
        .await
        .unwrap();
    server
        .send(ClientMessage::request(
            "r1".into(),
            "log.tail".into(),
            json!({"after_seq":1}),
            None,
        ))
        .await;
    let result = serde_json::to_value(server.recv().await).unwrap();
    assert_eq!(result["error"]["code"], "refused");
    assert!(result["error"]["message"].as_str().unwrap().contains('2'));
    for params in [
        json!({"limit":1001}),
        json!({"limits":2}),
        json!([]),
        json!({"after_seq":-1}),
        json!({"topics":"x"}),
    ] {
        server
            .send(ClientMessage::request(
                "r1".into(),
                "log.tail".into(),
                params,
                None,
            ))
            .await;
        assert_eq!(
            serde_json::to_value(server.recv().await).unwrap()["error"]["code"],
            "invalid_params"
        );
    }
    server.stop().await;
    cancel.cancel();
}

#[tokio::test]
async fn tail_id_with_no_room_for_even_an_error_closes_without_partial_frame() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let mut services = MethodServices::new(atmd::registry::spawn_registry());
    services.org = org;
    let mut server = SocketServer::start(services).await;
    let mut request = ClientMessage::request(String::new(), "log.tail".into(), json!({}), None);
    let base = serde_json::to_vec(&request).unwrap().len() + 1;
    if let atm_protocol::MessageType::Request { id, .. } = &mut request.message {
        *id = "i".repeat(1_048_576 - base);
    }
    assert_eq!(serde_json::to_vec(&request).unwrap().len() + 1, 1_048_576);
    server.send(request).await;
    let mut frame = String::new();
    assert_eq!(
        timeout(Duration::from_secs(2), server.read.read_line(&mut frame))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    server.stop().await;
    cancel.cancel();
}

#[tokio::test]
async fn watch_unwatch_keeps_frame_boundaries_but_shutdown_cancels_blocked_write() {
    for shutdown in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
        let mut services = MethodServices::new(atmd::registry::spawn_registry());
        services.org = org.clone();
        let mut server = SocketServer::start(services).await;
        org.execute(audit("x".repeat(4 * 1024 * 1024)), now())
            .await
            .unwrap();
        server.send(ClientMessage::watch(vec![], 0)).await;
        assert_eq!(
            timeout(Duration::from_secs(2), server.read.read_u8())
                .await
                .unwrap()
                .unwrap(),
            b'{'
        );
        if shutdown {
            server.cancel.cancel();
            timeout(Duration::from_millis(500), server.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            server.send(ClientMessage::unwatch()).await;
            server.send(ClientMessage::ping(42)).await;
            let mut frame = String::from("{");
            timeout(Duration::from_secs(2), server.read.read_line(&mut frame))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&frame).unwrap()["seq"],
                1
            );
            assert!(matches!(
                server.recv().await,
                DaemonMessage::Pong { seq: 42 }
            ));
            server.stop().await;
        }
        cancel.cancel();
    }
}

#[tokio::test]
async fn watch_replay_error_has_code_without_id_and_ping_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    org.execute(audit("one"), now()).await.unwrap();
    // A real post-start read fault, not a disabled-service setup refusal.
    std::fs::write(dir.path().join("log/events/00000001.jsonl"), b"broken\n").unwrap();
    let mut services = MethodServices::new(atmd::registry::spawn_registry());
    services.org = org;
    let mut server = SocketServer::start(services).await;
    server.send(ClientMessage::watch(vec![], 0)).await;
    let error = serde_json::to_value(server.recv().await).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "unavailable");
    assert!(error.get("id").is_none());
    assert!(error["message"].as_str().unwrap().contains("line 1"));
    server.send(ClientMessage::ping(8)).await;
    assert!(matches!(
        server.recv().await,
        DaemonMessage::Pong { seq: 8 }
    ));
    server.stop().await;
    cancel.cancel();
}

fn audit(text: impl Into<String>) -> OrgCommand {
    OrgCommand::ControlSent(atm_org::ControlAudit {
        pane: "%12".into(),
        action: json!({"text":text.into()}),
    })
}

#[tokio::test]
async fn watch_replay_live_overlap_reconnect_and_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    org.execute(project(), now()).await.unwrap();
    let mut watch = org.watch(vec![], 0).unwrap();
    // These writes occur after subscription but before replay: exactly once.
    org.execute(audit("two"), now()).await.unwrap();
    org.execute(audit("three"), now()).await.unwrap();
    for seq in 1..=3 {
        assert_eq!(
            timeout(Duration::from_secs(2), watch.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .seq,
            seq
        );
    }
    org.execute(audit("four"), now()).await.unwrap();
    assert_eq!(watch.next().await.unwrap().unwrap().seq, 4);
    let mut reconnect = org
        .watch(vec!["control".into(), "control.sent".into()], 2)
        .unwrap();
    assert_eq!(reconnect.next().await.unwrap().unwrap().seq, 3);
    assert_eq!(reconnect.next().await.unwrap().unwrap().seq, 4);
    assert!(timeout(Duration::from_millis(30), reconnect.next())
        .await
        .is_err());
    // Cancelling an idle next must not lose the next event.
    org.execute(audit("five"), now()).await.unwrap();
    assert_eq!(reconnect.next().await.unwrap().unwrap().seq, 5);
    cancel.cancel();
}

#[tokio::test]
async fn watch_lag_replays_without_gap() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let mut watch = org.watch(vec![], 0).unwrap();
    org.execute(audit("first"), now()).await.unwrap();
    assert_eq!(watch.next().await.unwrap().unwrap().seq, 1);
    // Exceed the actual EventLog broadcast capacity while not polling watch.
    for n in 2..=1100 {
        org.execute(audit(n.to_string()), now()).await.unwrap();
    }
    for seq in 2..=1100 {
        assert_eq!(
            timeout(Duration::from_secs(2), watch.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .seq,
            seq
        );
    }
    assert!(timeout(Duration::from_millis(30), watch.next())
        .await
        .is_err());
    cancel.cancel();
}

#[tokio::test]
async fn tail_pages_by_count_and_bytes_and_sparse_filters_progress() {
    use atmd::org::TailParams;
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().into(), cancel.clone()).await;
    for n in 0..600 {
        org.execute(audit(n.to_string()), now()).await.unwrap();
    }
    org.execute(project(), now()).await.unwrap();
    let sparse = org
        .tail(TailParams {
            topics: vec!["project".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(sparse.len(), 1);
    assert_eq!(sparse[0].seq, 601);
    let page = org
        .tail(TailParams {
            after_seq: 598,
            limit: 2,
            topics: vec![],
        })
        .await
        .unwrap();
    assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), [599, 600]);
    for _ in 0..3 {
        org.execute(audit("x".repeat(220_000)), now())
            .await
            .unwrap();
    }
    let page = org
        .tail(TailParams {
            after_seq: 601,
            limit: 1000,
            topics: vec![],
        })
        .await
        .unwrap();
    assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), [602, 603]);
    assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
    let page = org
        .tail(TailParams {
            after_seq: 603,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].seq, 604);
    org.execute(audit("x".repeat(600_000)), now())
        .await
        .unwrap();
    let err = org
        .tail(TailParams {
            after_seq: 604,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("605"));
    for limit in [0, 1001] {
        assert!(org
            .tail(TailParams {
                limit,
                ..Default::default()
            })
            .await
            .is_err());
    }
    cancel.cancel();
}

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

async fn reopen(path: &Path, cancel: CancellationToken) -> OrgHandle {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let org = OrgHandle::open(path.to_owned(), cancel.clone()).await;
            if org.status().is_ok() {
                return org;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("writer released the lock")
}

// Missing apply/replay or an incorrectly encoded cost loses these facts.
#[tokio::test]
async fn restart_replays_full_org_and_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    org.execute(project(), now()).await.unwrap();
    org.execute(
        OrgCommand::Adopt {
            agent_id: "demo/lead".parse().unwrap(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        },
        now(),
    )
    .await
    .unwrap();
    let summary = SessionSummary {
        session_id: SessionId::new("s1"),
        pane: Some("%1".into()),
        agent_id: None,
        harness: Harness::Pi,
        model: Some("example-model".into()),
        cost: Some(Money::from_microdollars(10_650)),
        first_prompt: Some("Read note.txt and reply.".into()),
        started: now(),
        ended: None,
    };
    org.execute(OrgCommand::RecordSummary(summary.clone()), now())
        .await
        .unwrap();
    assert!(org
        .execute(OrgCommand::RecordSummary(summary), now())
        .await
        .unwrap()
        .is_empty());
    let before = org.snapshot().await.unwrap();
    assert_eq!(
        before
            .session_summary(&SessionId::new("s1"))
            .unwrap()
            .agent_id,
        Some("demo/lead".parse().unwrap())
    );
    assert_eq!(org.status().unwrap(), 3);
    cancel.cancel();
    drop(org);
    let cancel = CancellationToken::new();
    let restarted = reopen(dir.path(), cancel.clone()).await;
    assert_eq!(restarted.snapshot().await.unwrap(), before);
    let rows: Vec<_> = read_from(dir.path(), Stream::Events, 0)
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].body["cost"], json!({"microdollars": 10650}));
    cancel.cancel();
}

// Deciding on two snapshots outside the actor would log conflicting pane owners.
#[tokio::test]
async fn concurrent_pane_claims_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    org.execute(project(), now()).await.unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for name in ["one", "two"] {
        let org = org.clone();
        tasks.spawn(async move {
            org.execute(
                OrgCommand::Adopt {
                    agent_id: format!("demo/{name}").parse().unwrap(),
                    pane: "%1".into(),
                    session_id: None,
                },
                now(),
            )
            .await
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        successes += usize::from(result.unwrap().is_ok());
    }
    assert_eq!(successes, 1);
    assert_eq!(org.snapshot().await.unwrap().agents().count(), 1);
    assert_eq!(read_from(dir.path(), Stream::Events, 0).count(), 2);
    cancel.cancel();
}

fn segment(dir: &Path, first: u64, bytes: &[u8]) {
    let path = dir.join("log/events");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join(format!("{first:08}.jsonl")), bytes).unwrap();
}

fn record(seq: u64, kind: &str, body: serde_json::Value) -> Vec<u8> {
    format!(
        "{}\n",
        json!({"seq":seq,"ts":now(),"kind":kind,"body":body})
    )
    .into_bytes()
}

#[tokio::test]
async fn old_segment_and_known_body_corruption_disable_org() {
    for old in [
        b"not json\n".to_vec(),
        record(
            1,
            "agent.down",
            json!({"agent_id":"absent/a","reason":"gone"}),
        ),
        record(1, "session.summary", json!({"cost":"invalid"})),
    ] {
        let dir = tempfile::tempdir().unwrap();
        segment(dir.path(), 1, &old);
        segment(dir.path(), 2, &record(2, "future.kind", json!({})));
        let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
        let error = org.snapshot().await.unwrap_err().to_string();
        assert!(!error.is_empty());
        assert!(org.execute(project(), now()).await.is_err());
        assert_eq!(
            std::fs::read(dir.path().join("log/events/00000001.jsonl")).unwrap(),
            old
        );
    }
}

#[tokio::test]
async fn newer_format_and_locked_log_are_clear_unavailable_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("log")).unwrap();
    std::fs::write(dir.path().join("log/format"), "2\n").unwrap();
    let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
    assert!(org
        .status()
        .unwrap_err()
        .to_string()
        .contains("unsupported log format"));
    let dir = tempfile::tempdir().unwrap();
    let _lock = EventLog::open(dir.path(), Stream::Events).unwrap();
    let org = OrgHandle::open(dir.path().to_owned(), CancellationToken::new()).await;
    assert!(org.status().unwrap_err().to_string().contains("locked"));
}

#[tokio::test]
async fn unknown_kind_advances_cursor_without_changing_org() {
    let dir = tempfile::tempdir().unwrap();
    let event = OrgEvent::ProjectRegistered {
        project_id: "demo".parse().unwrap(),
        root: "/demo".into(),
    };
    segment(
        dir.path(),
        1,
        &[
            record(1, event.kind(), event.to_body()),
            record(2, "future.kind", json!(null)),
        ]
        .concat(),
    );
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().to_owned(), cancel.clone()).await;
    assert_eq!(org.status().unwrap(), 2);
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    assert!(org.execute(project(), now()).await.unwrap().is_empty());
    assert_eq!(org.status().unwrap(), 2);
    cancel.cancel();
}
