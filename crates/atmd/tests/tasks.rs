//! Synthetic task-method contracts over committed state and a private server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{
    OrgCommand, OrgState, TaskActor, TaskCommand, TaskCreated, TaskState, TaskSubmission,
};
use atmd::{
    org::OrgHandle,
    registry::spawn_registry,
    server::{
        methods::{
            router, CallContext, CallerId, CallerPane, MethodError, MethodRouter, MethodServices,
        },
        DaemonServer,
    },
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

#[path = "support/task_policy.rs"]
mod policy;

fn now() -> DateTime<Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}
fn caller(pane: Option<&str>) -> CallContext {
    CallContext {
        client_id: CallerId("task-test".into()),
        from_pane: pane.map(|p| CallerPane(p.into())),
    }
}
fn actor(who: &str) -> TaskActor {
    TaskActor {
        who: who.parse().unwrap(),
        lead: who == "project/lead",
    }
}
fn created(title: &str) -> TaskCreated {
    serde_json::from_value(json!({"title":title,"kind":"work","creator":"owner","parent":null,"judges":null,"measure":{"kind":"mechanical","command":"check"},"budget":null,"files":[{"path":"briefs/task.md","kind":"brief","note":null}],"rests_on":null})).unwrap()
}
struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    router: MethodRouter,
    cancel: CancellationToken,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        org.execute(
            OrgCommand::RegisterProject {
                project_id: "project".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            now(),
        )
        .await
        .unwrap();
        for (name, pane) in [
            ("lead", "%1"),
            ("worker", "%2"),
            ("reviewer", "%3"),
            ("other", "%4"),
        ] {
            org.execute(
                OrgCommand::Adopt {
                    agent_id: format!("project/{name}").parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                },
                now(),
            )
            .await
            .unwrap();
        }
        org.execute(
            OrgCommand::Task(TaskCommand::Create(created("Task"))),
            now(),
        )
        .await
        .unwrap();
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        let router = router(services).unwrap();
        Self {
            dir,
            org,
            router,
            cancel,
        }
    }
    async fn run(&self, command: TaskCommand) {
        self.org
            .execute(OrgCommand::Task(command), now())
            .await
            .unwrap();
    }
    async fn assign(&self) {
        self.run(TaskCommand::Assign {
            task_id: "t6".parse().unwrap(),
            agent_id: "project/worker".parse().unwrap(),
            by: actor("owner"),
        })
        .await;
    }
    async fn submitted(&self) {
        self.assign().await;
        self.run(TaskCommand::Submit {
            task_id: "t6".parse().unwrap(),
            what: TaskSubmission::Result,
            files: vec![],
            by: actor("project/worker"),
        })
        .await;
    }
    async fn call(
        &self,
        pane: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, MethodError> {
        self.router.call(caller(pane), method, params).await
    }
    async fn ok(&self, pane: Option<&str>, method: &str, params: Value) -> Value {
        self.call(pane, method, params).await.unwrap()
    }
    async fn snapshot(&self) -> OrgState {
        self.org.snapshot().await.unwrap()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Break caught: a task method absent from actual wire dispatch, or schema/result drift.
#[tokio::test]
async fn all_nine_methods_success_and_refusal_wire_fixtures() {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tasks");
    let mut paths: Vec<_> = std::fs::read_dir(folder)
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 18);
    for path in paths {
        let f: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let rig = Rig::new().await;
        match f["setup"].as_str().unwrap_or("") {
            "assigned" => rig.assign().await,
            "submitted" => rig.submitted().await,
            "review" => {
                rig.submitted().await;
                let mut review = created("Review");
                review.kind = atm_org::TaskKind::Review;
                review.judges = Some("t6".parse().unwrap());
                rig.run(TaskCommand::Create(review)).await;
            }
            "" => (),
            other => panic!("bad setup: {other}"),
        }
        let before = rig.snapshot().await;
        let mut services = MethodServices::new(spawn_registry());
        services.org = rig.org.clone();
        services.agents = Some(atmd::org::agents::AgentHandle::disabled("fixture"));
        services.messages = Some(atmd::org::messages::MessageHandle::disabled("fixture"));
        let (control, control_task) = atmd::control::service::spawn_control(
            Box::new(atm_tmux::MockTmuxClient::new()),
            services.registry.clone(),
            rig.org.clone(),
            rig.cancel.clone(),
        );
        services.control = Some(control);
        let socket = rig.dir.path().join("tasks.sock");
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
                    b"{\"type\":\"connect\",\"protocol_version\":{\"major\":1,\"minor\":1}}\n",
                )
                .await
                .unwrap();
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            let hello: Value = serde_json::from_str(&line).unwrap();
            for method in [
                "task.create",
                "task.assign",
                "task.set_state",
                "task.submit",
                "task.attach",
                "task.review",
                "task.verdict",
                "task.list",
                "task.get",
            ] {
                assert!(
                    hello["methods"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(method)),
                    "{method}: {hello}"
                );
            }
            write
                .write_all(format!("{}\n", f["request"]).as_bytes())
                .await
                .unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
            let actual: Value = serde_json::from_str(&line).unwrap();
            if f["response"]["error"].is_object() {
                assert_eq!(
                    actual["error"]["code"],
                    f["response"]["error"]["code"],
                    "{}: {actual}",
                    path.display()
                );
                assert!(actual["error"]["message"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()));
                assert_eq!(
                    rig.snapshot().await,
                    before,
                    "refusal must not change state"
                );
            } else {
                let mut expected = f["response"].clone();
                // The production clock owns create timestamps; presence/format is checked, not guessed.
                if expected["result"]["task"]["created_at"] == "CURRENT" {
                    expected["result"]["task"]["created_at"] = json!(now());
                    expected["result"]["task"]["changed_at"] = json!(now());
                }
                assert_eq!(actual, expected, "{}", path.display());
            }
        };
        tokio::time::timeout(Duration::from_secs(60), check)
            .await
            .unwrap();
        rig.cancel.cancel();
        task.await.unwrap().unwrap();
        control_task.await.unwrap();
    }
}

// Break caught: duplicate assign/submit sends extra briefs, or report goes to manager not creator.
#[tokio::test]
async fn automatic_messages_name_task_and_preserve_actor_files_and_noops() {
    let rig = Rig::new().await;
    let assigned = rig
        .ok(
            Some("%1"),
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"}),
        )
        .await;
    assert_eq!(
        assigned,
        json!({"task_id":"t6","changed":true,"notification":{"status":"queued","message_id":"m8"}})
    );
    let state = rig.snapshot().await;
    let brief = state.message(&atm_org::MessageId::new(8)).unwrap();
    assert_eq!(
        serde_json::to_value(&brief.content).unwrap(),
        json!({"from":"project/lead","to":"project/worker","kind":"brief","body":"Task t6 assigned: Task\nInspect the task: atm task get t6\nRead the attached brief files before starting.","attachments":["briefs/task.md"],"task":"t6"})
    );
    assert_eq!(
        rig.ok(
            Some("%1"),
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"})
        )
        .await,
        json!({"task_id":"t6","changed":false,"notification":null})
    );
    assert_eq!(rig.snapshot().await, state);
    let files = json!([{"path":"report.md","kind":"report","note":"round one"},{"path":"report.md","kind":"evidence","note":null}]);
    let submitted = rig
        .ok(
            Some("%2"),
            "task.submit",
            json!({"task_id":"t6","files":files}),
        )
        .await;
    let id: atm_org::MessageId = submitted["notification"]["message_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let state = rig.snapshot().await;
    let report = state.message(&id).unwrap();
    assert_eq!(
        serde_json::to_value(&report.content).unwrap(),
        json!({"from":"project/worker","to":"owner","kind":"report","body":"Task t6 submitted (result): Task\nSubmitted by project/worker; inspect the task: atm task get t6\nRead the attached submission files before giving a verdict.","attachments":["report.md"],"task":"t6"})
    );
    assert_eq!(
        rig.ok(
            Some("%2"),
            "task.submit",
            json!({"task_id":"t6","files":files})
        )
        .await,
        json!({"task_id":"t6","changed":false,"notification":null})
    );
    assert_eq!(rig.snapshot().await, state);
}

// Break caught: authority inferred from client name/manager, owner allowed to submit, or own verdict allowed.
#[tokio::test]
async fn caller_attribution_authority_and_direct_states_are_enforced() {
    let rig = Rig::new().await;
    for pane in [Some("%2"), Some("%3"), Some("%4")] {
        assert_eq!(
            rig.call(
                pane,
                "task.assign",
                json!({"task_id":"t6","agent_id":"project/worker"})
            )
            .await
            .unwrap_err()
            .0
            .code,
            "refused"
        );
    }
    rig.ok(
        Some("%1"),
        "task.assign",
        json!({"task_id":"t6","agent_id":"project/worker"}),
    )
    .await;
    for pane in [None, Some("%1"), Some("%3")] {
        assert_eq!(
            rig.call(
                pane,
                "task.set_state",
                json!({"task_id":"t6","state":"working"})
            )
            .await
            .unwrap_err()
            .0
            .code,
            "refused"
        );
        assert_eq!(
            rig.call(pane, "task.submit", json!({"task_id":"t6"}))
                .await
                .unwrap_err()
                .0
                .code,
            "refused"
        );
    }
    for state in ["working", "waiting", "blocked"] {
        rig.ok(
            Some("%2"),
            "task.set_state",
            json!({"task_id":"t6","state":state,"reason":"needs input"}),
        )
        .await;
    }
    rig.ok(Some("%2"), "task.submit", json!({"task_id":"t6"}))
        .await;
    assert_eq!(
        rig.call(
            Some("%2"),
            "task.verdict",
            json!({"task_id":"t6","verdict":"accepted"})
        )
        .await
        .unwrap_err()
        .0
        .code,
        "refused"
    );
    rig.ok(
        Some("%99"),
        "task.verdict",
        json!({"task_id":"t6","verdict":"accepted"}),
    )
    .await;
    assert_eq!(
        rig.snapshot()
            .await
            .task(&"t6".parse().unwrap())
            .unwrap()
            .state,
        TaskState::Accepted
    );
    assert_eq!(
        rig.call(Some("%02"), "task.list", json!({}))
            .await
            .unwrap_err()
            .0
            .code,
        "invalid_params"
    );
}

// Break caught: task endpoints inherit TaskId's legacy free-text message references, or skip bounds/schema validation.
#[tokio::test]
async fn strict_schemas_ids_bounds_and_disabled_advertisement() {
    let disabled = router(MethodServices::new(spawn_registry())).unwrap();
    for (method, params) in [
        (
            "task.create",
            json!({"title":"x","measure":{"kind":"owner"}}),
        ),
        (
            "task.assign",
            json!({"task_id":"t0","agent_id":"project/worker"}),
        ),
        ("task.set_state", json!({"task_id":"t0","state":"working"})),
        ("task.submit", json!({"task_id":"t0"})),
        (
            "task.attach",
            json!({"task_id":"t0","file":{"path":"p","kind":"brief"}}),
        ),
        ("task.review", json!({"task_id":"t0","review_task":"t1"})),
        ("task.verdict", json!({"task_id":"t0","verdict":"accepted"})),
        ("task.list", json!({})),
        ("task.get", json!({"task_id":"t0"})),
    ] {
        assert!(disabled.names().contains(&method.into()));
        assert_eq!(
            disabled
                .call(caller(None), method, params.clone())
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable",
            "{method}"
        );
        let mut bad = params;
        bad["lead"] = json!(true);
        for bad in [bad, json!([]), json!(true)] {
            assert_eq!(
                disabled
                    .call(caller(None), method, bad)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "invalid_params",
                "{method}"
            );
        }
    }
    for id in ["t01", "t-1", "t+1", "t18446744073709551616", "work", "t1\n"] {
        assert_eq!(
            disabled
                .call(caller(None), "task.get", json!({"task_id":id}))
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params",
            "{id}"
        );
    }
    for (field, body) in [
        (
            "title",
            json!({"title":"é".repeat(513),"measure":{"kind":"owner"}}),
        ),
        (
            "files",
            json!({"title":"x","measure":{"kind":"owner"},"files":vec![json!({"path":"p","kind":"brief"});33]}),
        ),
    ] {
        assert_eq!(
            disabled
                .call(caller(None), "task.create", body)
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params",
            "{field}"
        );
    }
}
