//! Synthetic organisation fixtures: real committed files and RPC, no harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{Address, MessageContent, MessageId, MessageKind, OrgCommand};
use atm_protocol::{ClientMessage, DaemonMessage};
use atmd::{
    org::{agents::AgentHandle, messages::MessageHandle, OrgHandle},
    registry::spawn_registry,
    server::{
        methods::{router, CallContext, CallerId, CallerPane, MethodRouter, MethodServices},
        DaemonServer,
    },
};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    cancel: CancellationToken,
    router: MethodRouter,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for command in [
            OrgCommand::RegisterProject {
                project_id: "project".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            OrgCommand::Adopt {
                agent_id: "project/worker".parse().unwrap(),
                pane: "%7".into(),
                session_id: None,
            },
        ] {
            org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        let router = router(services).unwrap();
        Self {
            dir,
            org,
            cancel,
            router,
        }
    }
    fn role_path(&self) -> PathBuf {
        self.dir.path().join(".atm/roles/worker.md")
    }
    fn memory_path(&self) -> PathBuf {
        self.dir.path().join(".atm/memory/worker.md")
    }
    fn role(&self, text: &str) {
        std::fs::create_dir_all(self.role_path().parent().unwrap()).unwrap();
        std::fs::write(self.role_path(), text).unwrap();
    }
    async fn call(
        &self,
        pane: Option<&str>,
        params: Value,
    ) -> Result<Value, atmd::server::methods::MethodError> {
        self.router
            .call(
                CallContext {
                    client_id: CallerId("context-test".into()),
                    from_pane: pane.map(|p| CallerPane(p.into())),
                },
                "agent.context",
                params,
            )
            .await
    }
    async fn context(&self) -> Value {
        self.call(Some("%7"), json!({})).await.unwrap()
    }
    async fn send(&self, to: &str, body: &str) -> MessageId {
        let events = self
            .org
            .execute(
                OrgCommand::SendMessage(MessageContent {
                    from: Address::Owner,
                    to: to.parse().unwrap(),
                    kind: MessageKind::Brief,
                    body: body.into(),
                    attachments: vec![],
                    task: None,
                }),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        MessageId::new(events.first().unwrap().seq)
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Break caught: returning Owner's context, or accepting noncanonical pane ids.
#[tokio::test]
async fn context_refuses_unbound_missing_and_malformed_panes() {
    let rig = Rig::new().await;
    for pane in [None, Some("%9")] {
        let error = rig.call(pane, json!({})).await.unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert_eq!(
            error.0.message,
            "atm me requires a named agent bound to the caller's pane"
        );
    }
    for pane in ["%07", "7", "%", "%7\n"] {
        let error = rig.call(Some(pane), json!({})).await.unwrap_err();
        assert_eq!(error.0.code, "invalid_params");
    }
}

// Break caught: missing files incorrectly refusing, or falling back to cached roles.
#[tokio::test]
async fn context_adopted_agent_reads_fresh_role_and_manager() {
    let rig = Rig::new().await;
    let missing = rig.context().await;
    assert_eq!(missing["instructions"], Value::Null);
    assert_eq!(missing["manager"], "owner");
    assert_eq!(missing["role_file"], json!(rig.role_path()));
    assert_eq!(
        missing["report_command"],
        "atm msg send --to owner --kind report --body 'DONE: <result>'"
    );
    assert_eq!(missing["open_tasks"], json!([]));
    assert_eq!(
        missing["memory"],
        json!({"path":rig.memory_path(),"exists":false})
    );
    rig.role("+++\nharness='pi'\nmanager='lead'\n+++\nFirst instructions\n");
    let first = rig.context().await;
    assert_eq!(first["instructions"], "First instructions\n");
    assert_eq!(first["manager"], "project/lead");
    assert_eq!(
        first["report_command"],
        "atm msg send --to project/lead --kind report --body 'DONE: <result>'"
    );
    rig.role("+++\nharness='pi'\nmanager='other/lead'\n+++\nRevised instructions\n");
    let revised = rig.context().await;
    assert_eq!(revised["instructions"], "Revised instructions\n");
    assert_eq!(revised["manager"], "other/lead");
    std::fs::remove_file(rig.role_path()).unwrap();
    assert_eq!(rig.context().await, missing);
}

// Break caught: treating blank role instructions as a missing role, or misresolving managers.
#[tokio::test]
async fn context_manager_resolution_and_invalid_manager_refusal() {
    let rig = Rig::new().await;
    for (header, expected) in [
        ("", "owner"),
        ("manager='owner'", "owner"),
        ("manager='lead'", "project/lead"),
        ("manager='other/lead'", "other/lead"),
    ] {
        rig.role(&format!("+++\nharness='pi'\n{header}\n+++\n"));
        let value = rig.context().await;
        assert_eq!(value["manager"], expected);
        assert_eq!(value["instructions"], "");
    }
    for manager in [
        "",
        "system",
        "../lead",
        "Project/lead",
        "a/b/c",
        "lead;false",
    ] {
        rig.role(&format!(
            "+++\nharness='pi'\nmanager='{manager}'\n+++\nsecret body"
        ));
        let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
        assert_eq!(error.0.code, "refused", "{manager}");
        assert!(!error.0.message.contains("secret body"));
    }
    for text in [
        "not frontmatter secret body",
        "+++\nharness=17\n+++\nsecret body",
        "+++\nharness='pi'\nunknown='secret body'\n+++\n",
    ] {
        rig.role(text);
        let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert!(!error.0.message.contains("secret body"));
    }
}

// Break caught: truncating/filtering on delivery status, leaking another inbox, or auto-ack.
#[tokio::test]
async fn context_summarizes_only_unread_for_caller_without_side_effects() {
    let rig = Rig::new().await;
    rig.org
        .execute(
            OrgCommand::Adopt {
                agent_id: "project/other".parse().unwrap(),
                pane: "%8".into(),
                session_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let queued = rig.send("project/worker", "First line\nSecond line").await;
    rig.send("owner", "private owner").await;
    rig.send("project/other", "private other").await;
    let read = rig.send("project/worker", "already read").await;
    let failed = rig.send("project/worker", "\nblank first line").await;
    let empty = rig.send("project/worker", "").await;
    let delivering = rig.send("project/worker", "é🙂\r\nignored line").await;
    let delivered = rig.send("project/worker", "delivered unread").await;
    let events = rig
        .org
        .execute(
            OrgCommand::SendMessage(MessageContent {
                from: "project/other".parse().unwrap(),
                to: "project/worker".parse().unwrap(),
                kind: MessageKind::Report,
                body: "Member report".into(),
                attachments: vec![],
                task: None,
            }),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let report = MessageId::new(events.first().unwrap().seq);
    for id in [read, delivering, delivered] {
        rig.org
            .execute(
                OrgCommand::DeliveryAttempted {
                    message_id: id,
                    attempt: 1,
                    pane: "%7".into(),
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
    }
    for id in [read, delivered] {
        rig.org
            .execute(
                OrgCommand::Delivered {
                    message_id: id,
                    session_id: "session".into(),
                    confirmation: "synthetic".into(),
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
    }
    rig.org
        .execute(
            OrgCommand::ReadMessages {
                who: "project/worker".parse().unwrap(),
                message_ids: vec![read],
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    rig.org
        .execute(
            OrgCommand::DeliveryFailed {
                message_id: failed,
                reason: "synthetic failure".into(),
                final_failure: true,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let before = rig.org.snapshot().await.unwrap();
    let seq = rig.org.status().unwrap();
    for _ in 0..2 {
        assert_eq!(
            rig.context().await["unread_messages"],
            json!([
                {"message_id":queued,"kind":"brief","from":"owner","first_line":"First line"},
                {"message_id":failed,"kind":"brief","from":"owner","first_line":""},
                {"message_id":empty,"kind":"brief","from":"owner","first_line":""},
                {"message_id":delivering,"kind":"brief","from":"owner","first_line":"é🙂"},
                {"message_id":delivered,"kind":"brief","from":"owner","first_line":"delivered unread"},
                {"message_id":report,"kind":"report","from":"project/other","first_line":"Member report"}
            ])
        );
    }
    assert_eq!(rig.org.snapshot().await.unwrap(), before);
    assert_eq!(rig.org.status().unwrap(), seq);
}

// Break caught: cwd-relative paths and stale memory-existence caches.
#[tokio::test]
async fn context_uses_main_project_paths_and_live_memory_metadata() {
    let rig = Rig::new().await;
    assert_eq!(
        rig.context().await["memory"],
        json!({"path":rig.memory_path(),"exists":false})
    );
    std::fs::create_dir_all(rig.memory_path().parent().unwrap()).unwrap();
    std::fs::write(rig.memory_path(), "handover").unwrap();
    assert_eq!(rig.context().await["memory"]["exists"], true);
    std::fs::remove_file(rig.memory_path()).unwrap();
    std::fs::create_dir(rig.memory_path()).unwrap();
    assert_eq!(
        rig.call(Some("%7"), json!({})).await.unwrap_err().0.code,
        "refused"
    );
    std::fs::remove_dir(rig.memory_path()).unwrap();
    std::os::unix::fs::symlink("worker.md", rig.memory_path()).unwrap();
    let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
    assert_ne!(error.0.code, "unknown_method");
    assert!(error.0.message.contains("worker.md"));
}

// Break caught: unbounded role/result allocation and silently successful truncation.
#[tokio::test]
async fn context_role_and_result_limits_refuse_without_truncation() {
    let rig = Rig::new().await;
    let header = "+++\nharness='pi'\n+++\n";
    rig.role(&format!(
        "{header}{}",
        "a".repeat(256 * 1024 - header.len())
    ));
    assert_eq!(
        rig.context().await["instructions"].as_str().unwrap().len(),
        256 * 1024 - header.len()
    );
    rig.role(&format!(
        "{header}{}",
        "a".repeat(256 * 1024 + 1 - header.len())
    ));
    let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
    assert_eq!(error.0.code, "refused");
    assert!(error.0.message.contains("262144"));
    rig.role(&format!("{header}{}", "\u{0001}".repeat(100_000)));
    let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
    assert_eq!(error.0.code, "refused");
    assert!(error.0.message.contains("524288"));
    std::fs::remove_file(rig.role_path()).unwrap();
    for _ in 0..33 {
        rig.send("project/worker", &"x".repeat(16 * 1024)).await;
    }
    let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
    assert_eq!(error.0.code, "refused");
    assert!(error.0.message.contains("atm inbox --unread"));
}

// Break caught: invalid UTF-8 treated as an internal failure rather than an invalid role,
// or a size-limited read splitting a valid Unicode code point before checking the limit.
#[tokio::test]
async fn context_invalid_utf8_is_refused_and_unicode_role_limit_is_byte_based() {
    let rig = Rig::new().await;
    rig.role("+++\nharness='pi'\n+++\n");
    std::fs::write(rig.role_path(), b"+++\nharness='pi'\n+++\n\xff").unwrap();
    assert_eq!(
        rig.call(Some("%7"), json!({})).await.unwrap_err().0.code,
        "refused"
    );
    let header = "+++\nharness='pi'\n+++\n";
    rig.role(&format!(
        "{header}{}🙂",
        "a".repeat(256 * 1024 - header.len() - 1)
    ));
    let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
    assert_eq!(error.0.code, "refused");
    assert!(error.0.message.contains("262144"));
}

// Break caught: schema permissiveness or a missing advertised handler.
#[tokio::test]
async fn context_schema_is_strict_and_unavailable_is_explicit() {
    let rig = Rig::new().await;
    for params in [
        json!({"agent":"project/worker"}),
        json!([]),
        json!(true),
        json!("worker"),
    ] {
        assert_eq!(
            rig.call(Some("%7"), params).await.unwrap_err().0.code,
            "invalid_params"
        );
    }
    assert_eq!(
        rig.call(Some("%7"), Value::Null).await.unwrap(),
        rig.context().await
    );
    let disabled = router(MethodServices::new(spawn_registry())).unwrap();
    assert!(disabled.names().iter().any(|s| s == "agent.context"));
    let error = disabled
        .call(
            CallContext {
                client_id: CallerId("test".into()),
                from_pane: Some(CallerPane("%7".into())),
            },
            "agent.context",
            json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "unavailable");
}

// Break caught: method works locally but isn't registered/serialized over the wire.
#[tokio::test]
async fn context_success_and_refusal_conformance_fixtures() {
    let rig = Rig::new().await;
    rig.role("+++\nharness='pi'\nmanager='lead'\n+++\nFollow the brief.\n");
    assert_eq!(
        rig.send("project/worker", "Read the attached brief\nDetails")
            .await
            .to_string(),
        "m3"
    );
    let mut services = MethodServices::new(spawn_registry());
    services.org = rig.org.clone();
    services.agents = Some(AgentHandle::disabled("synthetic fixture"));
    services.messages = Some(MessageHandle::disabled("synthetic fixture"));
    let socket = rig.dir.path().join("context.sock");
    let server = DaemonServer::with_services(&socket, services, rig.cancel.clone());
    let mut run = Box::pin(async move { server.run().await });
    std::future::poll_fn(|cx| {
        use std::{future::Future, task::Poll};
        assert!(run.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let task = tokio::spawn(run);
    let check = async {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&ClientMessage::connect(None)).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        assert!(hello["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("agent.context")));
        for source in [
            include_str!("fixtures/agent_context/success.json"),
            include_str!("fixtures/agent_context/unbound.json"),
        ] {
            let mut fixture: Value = serde_json::from_str(source).unwrap();
            if fixture["response"]["result"].is_object() {
                fixture["response"]["result"]["role_file"] = json!(rig.role_path());
                fixture["response"]["result"]["memory"]["path"] = json!(rig.memory_path());
            }
            write
                .write_all(format!("{}\n", fixture["request"]).as_bytes())
                .await
                .unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                fixture["response"]
            );
        }
        write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&ClientMessage::ping(9)).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        line.clear();
        read.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::Pong { seq: 9 }
        ));
    };
    tokio::time::timeout(Duration::from_secs(60), check)
        .await
        .unwrap();
    rig.cancel.cancel();
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
