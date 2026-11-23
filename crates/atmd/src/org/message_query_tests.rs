//! History router contracts on a real fold with an injected in-memory appender.
use super::{reporting_line::ReportingFiles, OrgHandle, Request};
use crate::server::methods::{
    router_with_reporting, CallContext, CallerId, CallerPane, MethodServices,
};
use atm_org::{MessageContent, MessageId, MessageKind, OrgCommand, OrgEvent, OrgState};
use serde_json::{json, Value};
use std::{future::Future, task::Poll, time::Duration};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
const BYTES: usize = 512 * 1024;
fn now() -> chrono::DateTime<chrono::Utc> {
    "2026-01-15T00:00:00Z".parse().unwrap()
}
fn caller(pane: Option<&str>) -> CallContext {
    CallContext {
        client_id: CallerId("history-test".into()),
        from_pane: pane.map(|s| CallerPane(s.into())),
    }
}
fn content(from: &str, to: &str, body: &str) -> MessageContent {
    MessageContent {
        delivery_notice: None,
        from: from.parse().unwrap(),
        to: to.parse().unwrap(),
        kind: MessageKind::Report,
        body: body.into(),
        attachments: vec!["SECRET attachment".into()],
        task: Some("SECRET task".parse().unwrap()),
        post: None,
    }
}
fn row_ids(page: &Value) -> Vec<String> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_str().unwrap().into())
        .collect()
}
struct Rig {
    _dir: tempfile::TempDir,
    org: OrgHandle,
    files: ReportingFiles,
    snapshots: mpsc::Receiver<()>,
    cancel: CancellationToken,
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Rig {
    fn new(messages: Vec<MessageContent>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut state = OrgState::default();
        let mut seq = 0;
        let mut apply = |event| {
            seq += 1;
            state.apply(seq, now(), &event).unwrap();
        };
        for project in ["alpha", "beta", "gamma"] {
            let root = dir.path().join(project);
            std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
            apply(OrgEvent::ProjectRegistered {
                project_id: project.parse().unwrap(),
                root: root.to_str().unwrap().into(),
            });
        }
        assert!(!dir.path().join("gamma/.git").exists());
        for (id, pane, manager) in [
            ("alpha/worker", "%1", "lead"),
            ("alpha/lead", "%2", "gamma/chief"),
            ("gamma/chief", "%3", "owner"),
            ("beta/lead", "%4", "owner"),
        ] {
            apply(OrgEvent::AgentAdopted {
                agent_id: id.parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            });
            let (project, name) = id.split_once('/').unwrap();
            std::fs::write(
                dir.path()
                    .join(project)
                    .join(format!(".atm/roles/{name}.md")),
                format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nSECRET instructions\n"),
            )
            .unwrap();
        }
        for message in messages {
            apply(OrgEvent::MessageSent(message));
        }
        let cancel = CancellationToken::new();
        let (real, rx) = mpsc::channel(64);
        let (health, status) = watch::channel(Ok(seq));
        let (committed, _) = broadcast::channel(64);
        tokio::spawn(super::actor::run(
            state,
            rx,
            health,
            committed.clone(),
            cancel.clone(),
            move |_| {
                seq += 1;
                std::future::ready(Ok(seq))
            },
        ));
        let (requests, mut rx) = mpsc::channel(64);
        let (seen, snapshots) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                match request {
                    Request::SnapshotAt(reply) => {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        real.send(Request::SnapshotAt(tx)).await.unwrap();
                        let _ = reply.send(rx.await.unwrap());
                        let _ = seen.send(()).await;
                    }
                    other => {
                        if real.send(other).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            _dir: dir,
            org: OrgHandle {
                requests,
                health: status,
                committed,
                reads: None,
            },
            files: ReportingFiles::default(),
            snapshots,
            cancel,
        }
    }
    fn router(&self) -> crate::server::methods::MethodRouter {
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = self.org.clone();
        router_with_reporting(services, self.files.clone()).unwrap()
    }
}

// Break: failure to advertise, lax schemas, or consulting an unavailable fold before validation.
#[tokio::test]
async fn history_strict_params_and_disabled_service() {
    let router =
        crate::server::methods::router(MethodServices::new(crate::registry::spawn_registry()))
            .unwrap();
    assert!(router.names().iter().any(|n| n == "msg.history"));
    for p in [
        json!([]),
        json!(false),
        json!({"extra":true}),
        json!({"limit":0}),
        json!({"limit":1001}),
        json!({"limit":-1}),
        json!({"text":""}),
        json!({"text":"é".repeat(129)}),
        json!({"task":"é".repeat(129)}),
        json!({"cursor":"x".repeat(4097)}),
        json!({"agent":"worker"}),
        json!({"since":"yesterday"}),
        json!({"since":"2026-01-02T00:00:00Z","until":"2026-01-01T00:00:00Z"}),
    ] {
        assert_eq!(
            router
                .call(caller(None), "msg.history", p)
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params"
        );
    }
    for p in [Value::Null, json!({}), json!({"limit":1000})] {
        assert_eq!(
            router
                .call(caller(None), "msg.history", p)
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
    }
}

// Break: byte paging advances past lookahead, includes new sends, or freezes delivery/read facts.
#[tokio::test]
async fn history_byte_pages_are_authorized_fenced_and_current() {
    let large = "\u{0000}\"é".repeat(20_000);
    let mut messages = Vec::new();
    for n in 0..5 {
        messages.push(content("owner", "alpha/worker", &large));
        if n < 4 {
            messages.push(content("owner", "beta/lead", "invisible"));
        }
    }
    let rig = Rig::new(messages);
    let router = rig.router();
    let before = rig.org.snapshot().await.unwrap();
    let fence = rig.org.status().unwrap();
    let first = router
        .call(caller(Some("%1")), "msg.history", json!({}))
        .await
        .unwrap();
    assert_eq!(row_ids(&first), ["m16", "m14"]);
    assert!(serde_json::to_vec(&first).unwrap().len() <= BYTES);
    assert_eq!(first["visibility"], "full");
    assert!(first["next_cursor"].is_string());
    assert_eq!(rig.org.snapshot().await.unwrap(), before);
    assert_eq!(rig.org.status().unwrap(), fence);
    let target = MessageId::new(12);
    for command in [
        OrgCommand::SendMessage(content("owner", "alpha/worker", "new visible")),
        OrgCommand::SendMessage(content("owner", "beta/lead", "new invisible")),
        OrgCommand::DeliveryAttempted {
            message_id: target,
            attempt: 2,
            pane: "%1".into(),
        },
        OrgCommand::Delivered {
            message_id: target,
            session_id: "recorded-session".into(),
            confirmation: "SECRET receipt".into(),
        },
        OrgCommand::ReadMessages {
            who: "alpha/worker".parse().unwrap(),
            message_ids: vec![target],
        },
    ] {
        rig.org.execute(command, now()).await.unwrap();
    }
    let second = router
        .call(
            caller(Some("%1")),
            "msg.history",
            json!({"agent":"alpha/worker","cursor":first["next_cursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(row_ids(&second), ["m12", "m10"]);
    assert_eq!(second["snapshot_seq"], fence);
    assert_eq!(second["seq"], rig.org.status().unwrap());
    assert_eq!(second["messages"][0]["status"], "delivered");
    assert_eq!(second["messages"][0]["read_at"], "2026-01-15T00:00:00Z");
    assert_eq!(
        second["messages"][0]["delivery"]["confirmation"],
        "SECRET receipt"
    );
    assert!(second["messages"][0]["delivery"]["retry_at"].is_null());
    assert!(second["messages"][0]["delivery"]["blocked_by"].is_null());
    let third = router
        .call(
            caller(Some("%1")),
            "msg.history",
            json!({"cursor":second["next_cursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(row_ids(&third), ["m8"]);
    assert!(third["next_cursor"].is_null());
    let all: Vec<_> = [&first, &second, &third]
        .into_iter()
        .flat_map(row_ids)
        .collect();
    assert_eq!(all, ["m16", "m14", "m12", "m10", "m8"]);
    for page in [&second, &third] {
        assert!(serde_json::to_vec(page).unwrap().len() <= BYTES);
    }
}

// Break: result budget ignores JSON escaping/framing, skips/truncates an oversized first row.
#[tokio::test]
async fn history_exact_result_bound_and_first_oversize_refusal() {
    let expected = json!({"seq":8,"snapshot_seq":8,"visibility":"full","next_cursor":null,"messages":[{
        "message_id":"m8","sent_at":"2026-01-15T00:00:00Z","status":"queued","read_at":null,
        "content":{"from":"owner","to":"alpha/worker","kind":"report","body":"","attachments":["SECRET attachment"],"task":"SECRET task"},
        "delivery":{"phase":"queued","reason":null,"attempt":0,"pane":null,"session_id":null,"confirmation":null,"retry_at":null,"blocked_by":null}
    }]});
    let size = BYTES - serde_json::to_vec(&expected).unwrap().len();
    for (extra, want_ok) in [(0, true), (1, false)] {
        let body = "x".repeat(size + extra);
        let rig = Rig::new(vec![content("owner", "alpha/worker", &body)]);
        let result = rig
            .router()
            .call(caller(None), "msg.history", json!({}))
            .await;
        if want_ok {
            let page = result.unwrap();
            assert_eq!(serde_json::to_vec(&page).unwrap().len(), BYTES);
            assert_eq!(page["messages"][0]["content"]["body"], body);
        } else {
            assert_eq!(result.unwrap_err().0.code, "refused");
        }
    }
}

// Break: redaction after accounting, private text in cursors/errors, hidden filters on empty data.
#[tokio::test]
async fn history_metadata_reuses_redaction_before_paging() {
    for count in [0, 6] {
        let rig = Rig::new(
            (0..count)
                .map(|_| {
                    content(
                        "owner",
                        "alpha/worker",
                        &format!("SECRET {}", "\0".repeat(BYTES)),
                    )
                })
                .collect(),
        );
        if count > 0 {
            for command in [
                OrgCommand::DeliveryAttempted {
                    message_id: MessageId::new(8),
                    attempt: 2,
                    pane: "%1".into(),
                },
                OrgCommand::Delivered {
                    message_id: MessageId::new(8),
                    session_id: "recorded-session".into(),
                    confirmation: "SECRET receipt".into(),
                },
                OrgCommand::DeliveryFailed {
                    message_id: MessageId::new(9),
                    reason: format!("SECRET {}", "x".repeat(BYTES)),
                    final_failure: true,
                },
            ] {
                rig.org.execute(command, now()).await.unwrap();
            }
        }
        let router = rig.router();
        let page = router
            .call(
                caller(Some("%3")),
                "msg.history",
                json!({"agent":"alpha/worker"}),
            )
            .await
            .unwrap();
        assert_eq!(page["visibility"], "delivery");
        assert_eq!(page["messages"].as_array().unwrap().len(), count);
        assert!(page["next_cursor"].is_null());
        assert!(!page.to_string().contains("SECRET"));
        for filter in [
            json!({"agent":"alpha/worker","text":"no match"}),
            json!({"agent":"alpha/worker","task":"no match"}),
        ] {
            let error = router
                .call(caller(Some("%3")), "msg.history", filter)
                .await
                .unwrap_err();
            assert_eq!(error.0.code, "refused");
            assert!(!error.to_string().contains("SECRET"));
        }
        if count > 0 {
            let first = router
                .call(
                    caller(Some("%3")),
                    "msg.history",
                    json!({"agent":"alpha/worker","limit":1}),
                )
                .await
                .unwrap();
            assert!(!first.to_string().contains("SECRET"));
            let next = router
                .call(
                    caller(Some("%3")),
                    "msg.history",
                    json!({"agent":"alpha/worker","limit":1,"cursor":first["next_cursor"]}),
                )
                .await
                .unwrap();
            assert_eq!(row_ids(&next), ["m12"]);
        }
    }
}

// Break: history has its own reporting lane, holds the fold across I/O, or re-snapshots after admission.
#[tokio::test]
async fn history_shared_reporting_slot_does_not_block_owner_self_or_fold() {
    let mut rig = Rig::new(vec![content("owner", "alpha/worker", "SECRET old")]);
    let router = rig.router();
    let held = rig.files.slot.clone().acquire_owned().await.unwrap();
    let mut manager = Box::pin(router.call(
        caller(Some("%2")),
        "msg.history",
        json!({"agent":"alpha/worker"}),
    ));
    std::future::poll_fn(|cx| {
        assert!(manager.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    rig.snapshots.recv().await.unwrap();
    std::future::poll_fn(|cx| {
        assert!(manager.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    rig.org
        .execute(
            OrgCommand::SendMessage(content("owner", "alpha/worker", "SECRET new")),
            now(),
        )
        .await
        .unwrap();
    for pane in [None, Some("%1")] {
        let page = tokio::time::timeout(
            Duration::from_secs(60),
            router.call(caller(pane), "msg.history", json!({})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    }
    drop(held);
    let page = manager.await.unwrap();
    assert_eq!(row_ids(&page), ["m8"]);
    assert_eq!(page["seq"], 8);
    assert!(!page.to_string().contains("SECRET"));
}

// Break: timeout/unavailable from the shared reader is flattened to refusal or bypassed.
#[tokio::test]
async fn history_reporting_failures_preserve_classification() {
    let mut rig = Rig::new(vec![]);
    let router = rig.router();
    let held = rig.files.slot.clone().acquire_owned().await.unwrap();
    let mut read = Box::pin(router.call(
        caller(Some("%2")),
        "msg.history",
        json!({"agent":"alpha/worker"}),
    ));
    std::future::poll_fn(|cx| {
        assert!(read.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    rig.snapshots.recv().await.unwrap();
    std::future::poll_fn(|cx| {
        assert!(read.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(read.await.unwrap_err().0.code, "timeout");
    tokio::time::resume();
    drop(held);
    rig.files.slot.close();
    assert_eq!(
        router
            .call(
                caller(Some("%2")),
                "msg.history",
                json!({"agent":"alpha/worker"})
            )
            .await
            .unwrap_err()
            .0
            .code,
        "unavailable"
    );
}

// Break: restart rejects a valid cursor or uses folded message count instead of replay high-water.
#[test]
fn history_cursor_replays_unknown_highwater_after_process_restart() {
    const CHILD: &str = "ATM_TEST_HISTORY_RESTART";
    const TEST: &str =
        "org::message_query_tests::history_cursor_replays_unknown_highwater_after_process_restart";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let cancel = CancellationToken::new();
                let org = OrgHandle::open(root.join("state"), cancel.clone()).await;
                assert_eq!(org.status().unwrap(), 4);
                let mut services = MethodServices::new(crate::registry::spawn_registry());
                services.org = org.clone();
                let router = router_with_reporting(services, ReportingFiles::default()).unwrap();
                let saved = root.join("page.json");
                if saved.exists() {
                    let first: Value =
                        serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
                    org.execute(
                        OrgCommand::SendMessage(content("system", "owner", "new send")),
                        now(),
                    )
                    .await
                    .unwrap();
                    org.execute(
                        OrgCommand::ReadMessages {
                            who: atm_org::Address::Owner,
                            message_ids: vec![MessageId::new(1)],
                        },
                        now(),
                    )
                    .await
                    .unwrap();
                    let next = router
                        .call(
                            caller(None),
                            "msg.history",
                            json!({"limit":1,"cursor":first["next_cursor"]}),
                        )
                        .await
                        .unwrap();
                    assert_eq!(row_ids(&next), ["m1"]);
                    assert_eq!(next["seq"], 6);
                    assert_eq!(next["snapshot_seq"], 4);
                    assert!(next["messages"][0]["read_at"].is_string());
                    assert!(next["next_cursor"].is_null());
                } else {
                    let page = router
                        .call(caller(None), "msg.history", json!({"limit":1}))
                        .await
                        .unwrap();
                    assert_eq!(row_ids(&page), ["m3"]);
                    assert_eq!(page["seq"], 4);
                    assert_eq!(page["snapshot_seq"], 4);
                    std::fs::write(&saved, serde_json::to_vec(&page).unwrap()).unwrap();
                }
                cancel.cancel();
            });
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("state/log/events");
    std::fs::create_dir_all(&events).unwrap();
    std::fs::write(dir.path().join("state/log/format"), b"1\n").unwrap();
    let mut lines = String::new();
    for (seq, kind, body) in [
        (
            1,
            "msg.sent",
            serde_json::to_value(content("system", "owner", "first")).unwrap(),
        ),
        (2, "future.unknown", json!({"ignored":true})),
        (
            3,
            "msg.sent",
            serde_json::to_value(content("system", "owner", "second")).unwrap(),
        ),
        (4, "future.unknown", json!({"ignored":true})),
    ] {
        lines.push_str(&json!({"seq":seq,"ts":now(),"kind":kind,"body":body}).to_string());
        lines.push('\n');
    }
    std::fs::write(events.join("00000001.jsonl"), lines).unwrap();
    // Process exit releases every ledger lock, not a sleep or lock-readiness poll.
    for _ in 0..2 {
        let output = std::process::Command::new("timeout")
            .arg("60")
            .arg(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture"])
            .env(CHILD, dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

// Break: snapshot has no bound, or a closed actor reply becomes an empty success.
#[tokio::test(start_paused = true)]
async fn history_snapshot_deadline_and_channel_close() {
    for timeout in [true, false] {
        let (requests, mut receive) = mpsc::channel(1);
        let (_health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(1);
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = OrgHandle {
            requests,
            health: status,
            committed,
            reads: None,
        };
        let router = router_with_reporting(services, ReportingFiles::default()).unwrap();
        let mut read = Box::pin(router.call(caller(None), "msg.history", json!({})));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let Request::SnapshotAt(reply) = receive.recv().await.unwrap() else {
            panic!("not an atomic snapshot")
        };
        if timeout {
            tokio::time::advance(Duration::from_secs(5)).await;
            assert_eq!(read.await.unwrap_err().0.code, "timeout");
        } else {
            drop(reply);
            assert_eq!(read.await.unwrap_err().0.code, "unavailable");
        }
    }
}
