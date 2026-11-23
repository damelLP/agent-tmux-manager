//! Synthetic task-method contracts over committed state and a private server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{
    channel_query::{post, wake_for},
    Address, DeliveryStatus, Mention, Message, MessageId, OrgCommand, OrgState, TaskActor,
    TaskCommand, TaskCreated, TaskState, TaskSubmission,
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
        manager_of: None,
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
        std::fs::create_dir_all(dir.path().join(".atm/roles")).unwrap();
        for name in ["worker", "reviewer"] {
            std::fs::write(
                dir.path().join(format!(".atm/roles/{name}.md")),
                "+++\nharness='pi'\nmanager='lead'\n+++\nSynthetic role\n",
            )
            .unwrap();
        }
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
    async fn verdict_ready(&self, what: TaskSubmission, reviewed: bool) -> Option<atm_org::TaskId> {
        self.assign().await;
        self.run(TaskCommand::Submit {
            task_id: "t6".parse().unwrap(),
            what,
            files: vec![],
            by: actor("project/worker"),
        })
        .await;
        if !reviewed {
            return None;
        }
        let mut review = created("Independent review");
        review.kind = atm_org::TaskKind::Review;
        review.judges = Some("t6".parse().unwrap());
        self.run(TaskCommand::Create(review)).await;
        let review: atm_org::TaskId = format!("t{}", self.org.status().unwrap()).parse().unwrap();
        self.run(TaskCommand::Assign {
            task_id: review.clone(),
            agent_id: "project/reviewer".parse().unwrap(),
            by: actor("owner"),
        })
        .await;
        self.run(TaskCommand::Submit {
            task_id: review.clone(),
            what: TaskSubmission::Result,
            files: vec![],
            by: actor("project/reviewer"),
        })
        .await;
        self.run(TaskCommand::StartReview {
            task_id: "t6".parse().unwrap(),
            review_task: review.clone(),
            by: actor("owner"),
        })
        .await;
        Some(review)
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

// Verify the native post while returning the original representation for exact
// rollback comparisons: older message readers ignore only the added post field.
fn legacy_task_wake(state: &OrgState, message: &Message, channel: &str) -> Value {
    let id = message
        .content
        .post
        .expect("automatic wake must link its post");
    let post = &post(state, id).expect("linked post must be native").content;
    let m = &message.content;
    assert_eq!(post.channel.to_string(), channel);
    assert!(id < message.id);
    assert_eq!(post.author, m.from);
    assert_eq!(post.kind, m.kind);
    assert_eq!(post.body, m.body);
    assert_eq!(post.attachments, m.attachments);
    assert_eq!(post.task, m.task);
    assert_eq!(post.wake, std::slice::from_ref(&m.to));
    let mention = match &m.to {
        Address::Owner => Mention::Owner,
        Address::Agent(seat) => Mention::Seat(seat.clone()),
        Address::System => panic!("system cannot be woken"),
    };
    assert_eq!(post.mentions, [mention]);
    assert!(!post.route);
    assert_eq!(post.thread, None);
    assert_eq!(post.decision, None);
    assert_eq!(post.client_key, None);
    assert_eq!(wake_for(state, id, &m.to), Some(message.id));
    assert_eq!(
        state
            .messages()
            .filter(|m| m.content.post == Some(id))
            .count(),
        1
    );
    let mut legacy = m.clone();
    legacy.post = None;
    serde_json::to_value(legacy).unwrap()
}

fn queued_id(result: &Value) -> MessageId {
    assert_eq!(result["notification"]["status"], "queued", "{result}");
    serde_json::from_value(result["notification"]["message_id"].clone()).unwrap()
}

// Break caught: revocation strands the former holder, wakes the new one twice, or forges attribution.
#[tokio::test]
async fn ownership_changes_notify_previous_holder() {
    for state in [
        "assigned",
        "working",
        "waiting",
        "blocked",
        "changes_requested",
        "submitted",
        "in_review",
    ] {
        for change in ["assigned", "open", "cancelled"] {
            if matches!(state, "submitted" | "in_review") && change != "cancelled" {
                continue;
            }
            let rig = Rig::new().await;
            rig.assign().await;
            match state {
                "assigned" => (),
                "submitted" | "in_review" | "changes_requested" => {
                    rig.run(TaskCommand::Submit {
                        task_id: "t6".parse().unwrap(),
                        what: TaskSubmission::Result,
                        files: vec![],
                        by: actor("project/worker"),
                    })
                    .await;
                    if state == "changes_requested" {
                        rig.run(TaskCommand::Verdict {
                            task_id: "t6".parse().unwrap(),
                            verdict: atm_org::TaskVerdict::ChangesRequested,
                            review_task: None,
                            note: None,
                            by: actor("owner"),
                        })
                        .await;
                    } else if state == "in_review" {
                        let mut review = created("Unassigned review");
                        review.kind = atm_org::TaskKind::Review;
                        review.judges = Some("t6".parse().unwrap());
                        rig.run(TaskCommand::Create(review)).await;
                        let review_task =
                            format!("t{}", rig.org.status().unwrap()).parse().unwrap();
                        rig.run(TaskCommand::StartReview {
                            task_id: "t6".parse().unwrap(),
                            review_task,
                            by: actor("owner"),
                        })
                        .await;
                    }
                }
                _ => {
                    rig.ok(
                        Some("%2"),
                        "task.set_state",
                        json!({"task_id":"t6","state":state,"reason":"do not copy this"}),
                    )
                    .await;
                }
            }
            let (method, params) = if change == "assigned" {
                (
                    "task.assign",
                    json!({"task_id":"t6","agent_id":"project/other"}),
                )
            } else {
                (
                    "task.set_state",
                    json!({"task_id":"t6","state":change,"reason":"do not copy this"}),
                )
            };
            let result = rig.ok(None, method, params.clone()).await;
            assert_eq!(
                result["notification"]["status"], "queued",
                "missing loss notice for {state}->{change}: {result}"
            );
            let snapshot = rig.snapshot().await;
            let messages: Vec<_> = snapshot.messages().collect();
            let notes: Vec<_> = messages
                .iter()
                .filter(|m| m.content.kind == atm_org::MessageKind::Note)
                .collect();
            assert_eq!(
                notes.len(),
                1,
                "missing former-holder notice for {state}->{change}"
            );
            assert_eq!(
                legacy_task_wake(&snapshot, notes[0], "project"),
                json!({
                    "from":"owner","to":"project/worker","kind":"note","task":"t6","attachments":[],
                    "body":format!("Task t6 changed: {change}.\nInspect current state and assignee: atm task get t6\nDo not continue obsolete work or resume/clear on this note alone."),
                })
            );
            assert_eq!(messages.len(), if change == "assigned" { 2 } else { 1 });
            assert_eq!(
                snapshot
                    .message(
                        &result["notification"]["message_id"]
                            .as_str()
                            .unwrap()
                            .parse()
                            .unwrap()
                    )
                    .unwrap()
                    .content
                    .kind,
                if change == "assigned" {
                    atm_org::MessageKind::Brief
                } else {
                    atm_org::MessageKind::Note
                }
            );
            if change == "assigned" || change == "cancelled" || change == "open" {
                assert_eq!(
                    rig.ok(None, method, params).await["notification"],
                    Value::Null
                );
                assert_eq!(rig.snapshot().await, snapshot);
            }
        }
    }
    for creator in ["project/lead", "project/worker"] {
        let rig = Rig::new().await;
        let mut body = created("Creator revoke");
        body.creator = creator.parse().unwrap();
        rig.run(TaskCommand::Create(body)).await;
        let id = format!("t{}", rig.org.status().unwrap());
        rig.run(TaskCommand::Assign {
            task_id: id.parse().unwrap(),
            agent_id: "project/worker".parse().unwrap(),
            by: actor("owner"),
        })
        .await;
        rig.ok(
            None,
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
        let state = rig.snapshot().await;
        let recipients: Vec<_> = state
            .messages()
            .map(|m| {
                legacy_task_wake(&state, m, "project")["to"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            recipients,
            if creator == "project/lead" {
                vec!["project/worker", "project/lead"]
            } else {
                vec!["project/worker"]
            }
        );
    }
    // A holder/creator revoking its own work gets no self Note; an unassigned cancellation is quiet.
    for held in [false, true] {
        let rig = Rig::new().await;
        let mut body = created("Self revoke");
        body.creator = "project/worker".parse().unwrap();
        rig.run(TaskCommand::Create(body)).await;
        let id = format!("t{}", rig.org.status().unwrap());
        if held {
            rig.run(TaskCommand::Assign {
                task_id: id.parse().unwrap(),
                agent_id: "project/worker".parse().unwrap(),
                by: actor("owner"),
            })
            .await;
        }
        assert_eq!(
            rig.ok(
                Some("%2"),
                "task.set_state",
                json!({"task_id":id,"state":"cancelled"})
            )
            .await["notification"],
            Value::Null
        );
        assert_eq!(rig.snapshot().await.messages().count(), 0);
    }
}

// Break caught: every Working start wakes the creator, or a withdrawn Report remains actionable without invalidation.
#[tokio::test]
async fn withdrawal_is_not_every_work_start() {
    for (creator, to) in [
        ("owner", "owner"),
        ("system", "owner"),
        ("project/lead", "project/lead"),
        ("project/worker", "project/worker"),
    ] {
        let rig = Rig::new().await;
        let mut body = created("Withdrawn report");
        body.creator = creator.parse().unwrap();
        rig.run(TaskCommand::Create(body)).await;
        let id = format!("t{}", rig.org.status().unwrap());
        rig.run(TaskCommand::Assign {
            task_id: id.parse().unwrap(),
            agent_id: "project/worker".parse().unwrap(),
            by: actor("owner"),
        })
        .await;
        let working = json!({"task_id":id,"state":"working"});
        assert_eq!(
            rig.ok(Some("%2"), "task.set_state", working.clone()).await["notification"],
            Value::Null
        );
        assert_eq!(
            rig.ok(Some("%2"), "task.set_state", working.clone()).await["notification"],
            Value::Null
        );
        let submitted = rig
            .ok(
                Some("%2"),
                "task.submit",
                json!({"task_id":id,"files":[{"path":"old-report.md","kind":"report"}]}),
            )
            .await;
        let report = if creator == "project/worker" {
            assert_eq!(submitted["notification"], Value::Null);
            None
        } else {
            let report_id = submitted["notification"]["message_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            Some((
                report_id,
                rig.snapshot().await.message(&report_id).unwrap().clone(),
            ))
        };
        let result = rig.ok(Some("%2"), "task.set_state", working.clone()).await;
        let state = rig.snapshot().await;
        if let Some((report_id, report)) = report {
            assert_eq!(
                state.message(&report_id).unwrap(),
                &report,
                "withdrawal must not rewrite the old Report"
            );
            let legacy = legacy_task_wake(&state, &report, "project");
            assert_eq!(legacy["to"], to);
            if creator == "system" {
                assert!(legacy["body"]
                    .as_str()
                    .unwrap()
                    .ends_with("\nCreator is system; report routed to owner."));
            }
        }
        assert_eq!(
            state.task(&id.parse().unwrap()).unwrap().state,
            TaskState::Working
        );
        if creator == "project/worker" {
            assert_eq!(result["notification"], Value::Null);
            assert_eq!(state.messages().count(), 0);
        } else {
            assert_eq!(
                result["notification"]["status"], "queued",
                "missing withdrawal notice: {result}"
            );
            let note = state
                .message(
                    &result["notification"]["message_id"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                legacy_task_wake(
                    &state,
                    note,
                    if to == "owner" {
                        "project/worker+owner"
                    } else {
                        "project"
                    }
                ),
                json!({
                    "from":"project/worker","to":to,"kind":"note","task":id,"attachments":[],
                    "body":format!("Task {id} changed: submission_withdrawn.\nInspect current state and assignee: atm task get {id}\nDo not continue obsolete work or resume/clear on this note alone."),
                })
            );
            assert_eq!(state.messages().count(), 2);
        }
        assert_eq!(
            rig.ok(Some("%2"), "task.set_state", working).await["notification"],
            Value::Null
        );
        assert_eq!(rig.snapshot().await, state);
    }
}

// Break caught: a displaced live review keeps working, or final primary/secondary closure gets a resume wake.
#[tokio::test]
async fn displaced_live_review_gets_invalidation_not_resume() {
    for outcome in [
        "cancelled",
        "accepted",
        "changes_requested",
        "plan_accepted",
    ] {
        let rig = Rig::new().await;
        rig.assign().await;
        rig.run(TaskCommand::Submit {
            task_id: "t6".parse().unwrap(),
            what: if outcome == "plan_accepted" {
                TaskSubmission::Plan
            } else {
                TaskSubmission::Result
            },
            files: vec![],
            by: actor("project/worker"),
        })
        .await;
        let mut body = created("Live review");
        body.kind = atm_org::TaskKind::Review;
        body.judges = Some("t6".parse().unwrap());
        rig.run(TaskCommand::Create(body)).await;
        let review = format!("t{}", rig.org.status().unwrap());
        rig.run(TaskCommand::Assign {
            task_id: review.parse().unwrap(),
            agent_id: "project/reviewer".parse().unwrap(),
            by: actor("owner"),
        })
        .await;
        rig.run(TaskCommand::SetState {
            task_id: review.parse().unwrap(),
            state: TaskState::Working,
            reason: None,
            by: actor("project/reviewer"),
        })
        .await;
        rig.run(TaskCommand::StartReview {
            task_id: "t6".parse().unwrap(),
            review_task: review.parse().unwrap(),
            by: actor("owner"),
        })
        .await;
        let before_review = rig
            .snapshot()
            .await
            .task(&review.parse().unwrap())
            .unwrap()
            .clone();
        let (method, params) = if outcome == "cancelled" {
            (
                "task.set_state",
                json!({"task_id":"t6","state":"cancelled"}),
            )
        } else {
            (
                "task.verdict",
                json!({"task_id":"t6","verdict":if outcome == "plan_accepted" { "accepted" } else { outcome },"note":"not copied"}),
            )
        };
        let result = rig.ok(Some("%1"), method, params.clone()).await;
        assert_eq!(
            result["notification"]["status"], "queued",
            "missing displaced review invalidation: {result}"
        );
        let state = rig.snapshot().await;
        assert_eq!(
            state.task(&review.parse().unwrap()).unwrap(),
            &before_review,
            "never cancel/close the live review automatically"
        );
        assert_eq!(
            state.task(&"t6".parse().unwrap()).unwrap().review_task,
            None
        );
        let review_notes: Vec<_> = state
            .messages()
            .filter(|m| {
                m.content
                    .task
                    .as_ref()
                    .is_some_and(|t| t.to_string() == review)
            })
            .collect();
        assert_eq!(review_notes.len(), 1, "missing linked live reviewer Note");
        assert_eq!(
            legacy_task_wake(&state, review_notes[0], "project"),
            json!({
                "from":"project/lead","to":"project/reviewer","kind":"note","task":review,"attachments":[],
                "body":format!("Task t6 changed: review_superseded.\nInspect current state and assignee: atm task get t6\nDo not continue obsolete work or resume/clear on this note alone.\nReview task: atm task get {review}; re-check the judged task above before continuing."),
            })
        );
        let primary: Vec<_> = state
            .messages()
            .filter(|m| {
                m.content
                    .task
                    .as_ref()
                    .is_some_and(|t| t.to_string() == "t6")
            })
            .collect();
        assert_eq!(
            primary.len(),
            if outcome == "accepted" {
                0
            } else if outcome == "cancelled" {
                2
            } else {
                1
            }
        );
        if matches!(outcome, "changes_requested" | "plan_accepted") {
            assert_eq!(primary[0].content.kind, atm_org::MessageKind::Answer);
        }
        if outcome == "plan_accepted" {
            assert_eq!(
                rig.call(Some("%1"), method, params)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "conflict"
            );
        } else {
            assert_eq!(
                rig.ok(Some("%1"), method, params).await["notification"],
                Value::Null
            );
        }
        assert_eq!(rig.snapshot().await, state);
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
            "accepted" => {
                rig.submitted().await;
                rig.run(TaskCommand::Verdict {
                    task_id: "t6".parse().unwrap(),
                    verdict: atm_org::TaskVerdict::Accepted,
                    review_task: None,
                    note: None,
                    by: actor("owner"),
                })
                .await;
            }
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
    let cursor = rig.org.status().unwrap();
    let assigned = rig
        .ok(
            None,
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"}),
        )
        .await;
    assert_eq!(assigned["task_id"], "t6");
    assert_eq!(assigned["changed"], true);
    let state = rig.snapshot().await;
    let brief = state.message(&queued_id(&assigned)).unwrap();
    assert_eq!(
        legacy_task_wake(&state, brief, "project"),
        json!({"from":"owner","to":"project/worker","kind":"brief","body":"Task t6 assigned: Task\nInspect the task: atm task get t6\nRead the attached brief files before starting.","attachments":["briefs/task.md"],"task":"t6"})
    );
    let records = rig
        .org
        .tail(atmd::org::TailParams {
            after_seq: cursor,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        records.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["task.assigned", "channel.posted", "msg.sent"]
    );
    assert_eq!(records[1].body["wake"], json!(["project/worker"]));
    assert_eq!(records[2].body["post"], json!(brief.content.post));
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
        legacy_task_wake(&state, report, "project"),
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

// Break caught: non-final verdicts strand a member or lose actor/task/review attribution.
#[tokio::test]
async fn nonfinal_verdicts_queue_one_actor_signed_answer() {
    for (what, verdict, wanted_state) in [
        (TaskSubmission::Plan, "accepted", TaskState::Working),
        (
            TaskSubmission::Plan,
            "changes_requested",
            TaskState::ChangesRequested,
        ),
        (
            TaskSubmission::Result,
            "changes_requested",
            TaskState::ChangesRequested,
        ),
    ] {
        for reviewed in [false, true] {
            for (pane, from) in [(None, "owner"), (Some("%1"), "project/lead")] {
                let rig = Rig::new().await;
                let review = rig.verdict_ready(what, reviewed).await;
                let result = rig
                    .ok(
                        pane,
                        "task.verdict",
                        json!({
                            "task_id":"t6", "verdict":verdict, "review_task":review,
                            "note":"Checked é🙂; read the evidence",
                        }),
                    )
                    .await;
                assert_eq!(result["changed"], true);
                assert_eq!(
                    result["notification"]["status"], "queued",
                    "missing verdict answer: {result}"
                );
                let id = result["notification"]["message_id"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                let state = rig.snapshot().await;
                let task = state.task(&"t6".parse().unwrap()).unwrap();
                assert_eq!(task.state, wanted_state);
                assert_eq!(task.submission, None);
                assert_eq!(
                    task.reason.as_deref(),
                    Some("Checked é🙂; read the evidence")
                );
                let mut body = format!("Task t6 verdict recorded: {verdict}.\nRe-read the current task and verdict note before acting: atm task get t6\nResume only as its current state and approved brief allow.");
                if let Some(review) = &review {
                    body.push_str(&format!("\nReview evidence: atm task get {review}"));
                    assert_eq!(state.task(review).unwrap().state, TaskState::Accepted);
                }
                assert_eq!(
                    legacy_task_wake(&state, state.message(&id).unwrap(), "project"),
                    json!({
                        "from":from,"to":"project/worker","kind":"answer","body":body,
                        "attachments":[],"task":"t6",
                    })
                );
                assert_eq!(
                    state.messages().count(),
                    1,
                    "secondary review must not wake its reviewer"
                );
            }
        }
    }
}

// Break caught: repeated verdicts duplicate wakes, or final acceptance/review bookkeeping wakes seats.
#[tokio::test]
async fn verdict_noops_finals_and_review_closure_do_not_duplicate_answers() {
    for (verdict, expected_messages) in [("changes_requested", 1), ("accepted", 0)] {
        let rig = Rig::new().await;
        let review = rig.verdict_ready(TaskSubmission::Result, true).await;
        let params = json!({"task_id":"t6","verdict":verdict,"review_task":review});
        let first = rig.ok(Some("%1"), "task.verdict", params.clone()).await;
        assert_eq!(first["changed"], true);
        if expected_messages == 0 {
            assert_eq!(first["notification"], Value::Null);
        } else {
            assert_eq!(
                first["notification"]["status"], "queued",
                "missing verdict answer"
            );
        }
        let before = rig.snapshot().await;
        assert_eq!(before.messages().count(), expected_messages);
        assert_eq!(
            before.task(review.as_ref().unwrap()).unwrap().state,
            TaskState::Accepted
        );
        assert_eq!(
            rig.ok(Some("%1"), "task.verdict", params).await,
            json!({"task_id":"t6","changed":false,"notification":null})
        );
        assert_eq!(rig.snapshot().await, before);
    }
}

// Break caught: a seat containing "lead" gains authority over a task it did not create.
#[tokio::test]
async fn near_lead_names_cannot_manage_another_creators_task() {
    for name in ["leader", "lead-b"] {
        for (method, mut params, submitted) in [
            (
                "task.assign",
                json!({"task_id":"t6","agent_id":"project/reviewer"}),
                false,
            ),
            ("task.review", json!({"task_id":"t6"}), true),
            (
                "task.set_state",
                json!({"task_id":"t6","state":"open"}),
                false,
            ),
            (
                "task.set_state",
                json!({"task_id":"t6","state":"cancelled"}),
                false,
            ),
            (
                "task.verdict",
                json!({"task_id":"t6","verdict":"accepted"}),
                true,
            ),
        ] {
            let rig = Rig::new().await;
            rig.org
                .execute(
                    OrgCommand::Adopt {
                        agent_id: format!("project/{name}").parse().unwrap(),
                        pane: "%5".into(),
                        session_id: None,
                    },
                    now(),
                )
                .await
                .unwrap();
            if submitted {
                rig.submitted().await;
            } else {
                rig.assign().await;
            }
            if method == "task.review" {
                let review = rig
                    .ok(
                        None,
                        "task.create",
                        json!({"title":"Review","kind":"review","judges":"t6","measure":{"kind":"owner"}}),
                    )
                    .await;
                params["review_task"] = review["task_id"].clone();
            }
            let before = rig.snapshot().await;
            let cursor = rig.org.status().unwrap();
            let result = rig.call(Some("%5"), method, params.clone()).await;
            assert!(
                matches!(&result, Err(error) if error.0.code == "refused"),
                "project/{name} {method} {params}: {result:?}"
            );
            assert_eq!(rig.snapshot().await, before);
            assert_eq!(rig.org.status().unwrap(), cursor);
            // The same request is valid for authority; refusal was not a bad fixture.
            assert_eq!(rig.ok(None, method, params).await["changed"], true);
        }
    }
}

// Break caught: task self-assignment/submission writes messages or replay invents them.
#[tokio::test]
async fn self_task_mutations_commit_without_message_events() {
    let rig = Rig::new().await;
    let cursor = rig.org.status().unwrap();
    let created = rig.ok(Some("%2"), "task.create", json!({"title":"Own task","measure":{"kind":"owner"},"files":[{"path":"brief.md","kind":"brief"}]})).await;
    let id: atm_org::TaskId = serde_json::from_value(created["task_id"].clone()).unwrap();
    let assign = json!({"task_id":id,"agent_id":"project/worker"});
    let submit = json!({"task_id":id,"files":[{"path":"report.md","kind":"report"}]});
    for (method, params) in [("task.assign", assign), ("task.submit", submit)] {
        let first = rig.ok(Some("%2"), method, params.clone()).await;
        assert_eq!(first["changed"], true);
        assert_eq!(first["notification"], Value::Null, "self enqueue: {first}");
        let before = rig.snapshot().await;
        let again = rig.ok(Some("%2"), method, params).await;
        assert_eq!(again["changed"], false);
        assert_eq!(again["notification"], Value::Null);
        assert_eq!(rig.snapshot().await, before);
    }
    let state = rig.snapshot().await;
    assert_eq!(state.messages().count(), 0);
    assert_eq!(state.task(&id).unwrap().state, TaskState::Submitted);
    assert_eq!(rig.org.status().unwrap(), cursor + 3);
    let events = rig
        .org
        .tail(atmd::org::TailParams {
            after_seq: cursor,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        ["task.created", "task.assigned", "task.submitted"]
    );
    for event in events.iter().skip(1) {
        assert_eq!(event.body["by"], "project/worker");
    }
    assert_eq!(events.last().unwrap().body["files"][0]["path"], "report.md");
    fn copy_log(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_log(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    // Manual self messages are still valid, and become historical records
    // for the replay check; suppression belongs only to the task composer.
    for kind in [atm_org::MessageKind::Brief, atm_org::MessageKind::Report] {
        rig.org
            .execute(
                OrgCommand::SendMessage(atm_org::MessageContent {
                    delivery_notice: None,
                    from: "project/worker".parse().unwrap(),
                    to: "project/worker".parse().unwrap(),
                    kind,
                    body: "Explicit self message".into(),
                    attachments: vec![],
                    task: Some(id.clone()),
                    post: None,
                }),
                now(),
            )
            .await
            .unwrap();
    }
    let state = rig.snapshot().await;
    assert_eq!(state.messages().count(), 2);
    // All mutation receipts completed; copy quiescent committed data to give
    // the replay lifetime its own writer lock, without any shutdown polling.
    let next = tempfile::tempdir().unwrap();
    copy_log(&rig.dir.path().join("state"), next.path());
    let cancel = CancellationToken::new();
    let reopened = OrgHandle::open(next.path().into(), cancel.clone()).await;
    assert_eq!(reopened.snapshot().await.unwrap(), state);
    assert_eq!(reopened.status().unwrap(), cursor + 5);
    cancel.cancel();
}

// Break caught: reports on agent-created tasks are always routed to the owner.
#[tokio::test]
async fn submitted_report_goes_to_agent_creator_not_assigning_owner() {
    let rig = Rig::new().await;
    let task = rig
        .ok(
            Some("%1"),
            "task.create",
            json!({"title":"Agent task","measure":{"kind":"owner"}}),
        )
        .await;
    let id = &task["task_id"];
    rig.ok(
        None,
        "task.assign",
        json!({"task_id":id,"agent_id":"project/worker"}),
    )
    .await;
    let submitted = rig
        .ok(Some("%2"), "task.submit", json!({"task_id":id}))
        .await;
    let message_id = submitted["notification"]["message_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let state = rig.snapshot().await;
    let report = state.message(&message_id).unwrap();
    assert_eq!(
        legacy_task_wake(&state, report, "project"),
        json!({"from":"project/worker","to":"project/lead","kind":"report","body":"Task t7 submitted (result): Agent task\nSubmitted by project/worker; inspect the task: atm task get t7\nNo submission file is attached.","attachments":[],"task":"t7"})
    );
}

// Break caught: assignment briefs attach report/evidence/plan/contract files too.
#[tokio::test]
async fn automatic_brief_attaches_only_brief_files() {
    let rig = Rig::new().await;
    for kind in ["report", "evidence", "plan", "contract"] {
        rig.ok(
            None,
            "task.attach",
            json!({"task_id":"t6","file":{"path":format!("{kind}.md"),"kind":kind}}),
        )
        .await;
    }
    let assigned = rig
        .ok(
            None,
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"}),
        )
        .await;
    let message_id = assigned["notification"]["message_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let state = rig.snapshot().await;
    let brief = state.message(&message_id).unwrap();
    assert_eq!(
        legacy_task_wake(&state, brief, "project")["attachments"],
        json!(["briefs/task.md"])
    );
}

async fn register_seat(rig: &Rig, project: &str, name: &str) -> String {
    let root = rig.dir.path().join(project);
    std::fs::create_dir(&root).unwrap();
    rig.org
        .execute(
            OrgCommand::RegisterProject {
                project_id: project.parse().unwrap(),
                root: root.to_str().unwrap().into(),
                move_root: false,
            },
            now(),
        )
        .await
        .unwrap();
    let seat = format!("{project}/{name}");
    rig.org
        .execute(
            OrgCommand::Adopt {
                agent_id: seat.parse().unwrap(),
                pane: "%5".into(),
                session_id: None,
            },
            now(),
        )
        .await
        .unwrap();
    seat
}

// Break caught: sender posts as a nonmember, short same-name tags are ambiguous,
// or an external creator receives the report in the creator's channel.
#[tokio::test]
async fn cross_project_task_brief_falls_back_to_direct_and_report_uses_submitter_project() {
    let rig = Rig::new().await;
    let other = register_seat(&rig, "remote", "lead").await;
    let task = rig
        .ok(
            Some("%1"),
            "task.create",
            json!({
                "title":"Cross-project work @owner", "measure":{"kind":"owner"},
            }),
        )
        .await;
    let id = &task["task_id"];
    let brief = queued_id(
        &rig.ok(
            Some("%1"),
            "task.assign",
            json!({"task_id":id,"agent_id":other}),
        )
        .await,
    );
    let report = queued_id(
        &rig.ok(Some("%5"), "task.submit", json!({"task_id":id}))
            .await,
    );
    let state = rig.snapshot().await;
    let brief = legacy_task_wake(
        &state,
        state.message(&brief).unwrap(),
        "project/lead+remote/lead",
    );
    let report = legacy_task_wake(&state, state.message(&report).unwrap(), "remote");
    assert_eq!(brief["to"], other);
    assert_eq!(report["to"], "project/lead");
    assert!(brief["body"]
        .as_str()
        .unwrap()
        .contains("Cross-project work @owner"));
    assert!(report["body"]
        .as_str()
        .unwrap()
        .contains("Cross-project work @owner"));
    assert_eq!(
        state.messages().count(),
        2,
        "copied title must not wake the owner"
    );
}

// Break caught: a queued wake depends on a live recipient or an unlisted channel.
#[tokio::test]
async fn task_post_to_down_seat_and_to_unlisted_project() {
    for listed in [true, false] {
        let rig = Rig::new().await;
        let seat = if listed {
            rig.org
                .execute(
                    OrgCommand::Down {
                        agent_id: "project/worker".parse().unwrap(),
                        reason: "fixture".into(),
                    },
                    now(),
                )
                .await
                .unwrap();
            "project/worker".to_owned()
        } else {
            register_seat(&rig, "remote", "worker").await
        };
        let row = queued_id(
            &rig.ok(None, "task.assign", json!({"task_id":"t6","agent_id":seat}))
                .await,
        );
        let state = rig.snapshot().await;
        let row = state.message(&row).unwrap();
        assert_eq!(row.status, DeliveryStatus::Queued);
        assert_eq!(
            legacy_task_wake(
                &state,
                row,
                if listed {
                    "project"
                } else {
                    "remote/worker+owner"
                }
            )["to"],
            seat
        );
    }
}

// Break caught: bad automatic content rolls the task back or leaves a post the repair can wake.
#[tokio::test]
async fn oversize_automatic_brief_commits_task_but_neither_post_nor_wake() {
    let rig = Rig::new().await;
    for n in 0..32 {
        rig.ok(
            None,
            "task.attach",
            json!({"task_id":"t6","file":{"path":format!("extra-{n}.md"),"kind":"brief"}}),
        )
        .await;
    }
    let cursor = rig.org.status().unwrap();
    let result = rig
        .ok(
            None,
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"}),
        )
        .await;
    assert_eq!(result["changed"], true);
    assert_eq!(result["notification"]["status"], "unconfirmed");
    let error = &result["notification"]["error"];
    assert_eq!(error["code"], "invalid_params");
    let desc = error["message"].as_str().unwrap();
    assert!(desc.contains("at most 32 attachments are allowed"));
    assert!(desc.contains("inspect the task and inbox before resending"));
    assert_eq!(
        rig.snapshot()
            .await
            .task(&"t6".parse().unwrap())
            .unwrap()
            .state,
        TaskState::Assigned
    );
    let events = rig
        .org
        .tail(atmd::org::TailParams {
            after_seq: cursor,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["task.assigned"]
    );
}

// Break caught: name-only task authority, owner allowed to submit, or own verdict allowed.
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
        None,
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

// Break caught: a replay-only enum fallback reaches the task worker (or a list query).
// The disabled router distinguishes RPC-edge validation from atm-org's later refusal.
async fn assert_unknown_task_param(
    method: &str,
    params: Value,
    pointer: &str,
    field: &str,
    pane: Option<&str>,
) {
    let rig = Rig::new().await;
    match method {
        "task.set_state" | "task.submit" => rig.assign().await,
        "task.verdict" => rig.submitted().await,
        _ => (),
    }
    let disabled = router(MethodServices::new(spawn_registry())).unwrap();
    let before = rig.snapshot().await;
    let cursor = rig.org.status().unwrap();
    for word in ["bogus", "unknown"] {
        let mut bad = params.clone();
        *bad.pointer_mut(pointer).unwrap() = json!(word);
        let result = rig.call(pane, method, bad.clone()).await;
        assert_eq!(rig.snapshot().await, before, "{method} {bad}");
        assert_eq!(rig.org.status().unwrap(), cursor, "{method} {bad}");
        let error = result.unwrap_err().0;
        assert_eq!(error.code, "invalid_params", "{method} {bad}: {error:?}");
        assert!(error.message.contains(field), "{method} {bad}: {error:?}");
        let error = disabled
            .call(caller(pane), method, bad.clone())
            .await
            .unwrap_err()
            .0;
        assert_eq!(error.code, "invalid_params", "{method} {bad}: {error:?}");
        assert!(error.message.contains(field), "{method} {bad}: {error:?}");
    }
    // The unchanged fixture is valid, so a policy refusal cannot mask the check.
    let result = rig.ok(pane, method, params).await;
    if method == "task.list" {
        assert_eq!(result["tasks"].as_array().unwrap().len(), 1);
    } else {
        assert_eq!(result["changed"], true);
    }
}

macro_rules! unknown_task_param_test {
    ($name:ident, $method:literal, $params:expr, $pointer:literal, $field:literal, $pane:expr) => {
        #[tokio::test]
        async fn $name() {
            assert_unknown_task_param($method, $params, $pointer, $field, $pane).await;
        }
    };
}
unknown_task_param_test!(
    unknown_create_kind_is_invalid_params,
    "task.create",
    json!({"title":"Known task","kind":"work","measure":{"kind":"owner"}}),
    "/kind",
    "kind",
    None
);
unknown_task_param_test!(
    unknown_create_measure_is_invalid_params,
    "task.create",
    json!({"title":"Known task","measure":{"kind":"owner"}}),
    "/measure/kind",
    "measure.kind",
    None
);
unknown_task_param_test!(
    unknown_create_file_kind_is_invalid_params,
    "task.create",
    json!({"title":"Known task","measure":{"kind":"owner"},"files":[{"path":"brief.md","kind":"brief"}]}),
    "/files/0/kind",
    "kind",
    None
);
unknown_task_param_test!(
    unknown_set_state_is_invalid_params,
    "task.set_state",
    json!({"task_id":"t6","state":"working"}),
    "/state",
    "state",
    Some("%2")
);
unknown_task_param_test!(
    unknown_submit_what_is_invalid_params,
    "task.submit",
    json!({"task_id":"t6","what":"result"}),
    "/what",
    "what",
    Some("%2")
);
unknown_task_param_test!(
    unknown_submit_file_kind_is_invalid_params,
    "task.submit",
    json!({"task_id":"t6","files":[{"path":"report.md","kind":"report"}]}),
    "/files/0/kind",
    "kind",
    Some("%2")
);
unknown_task_param_test!(
    unknown_attach_file_kind_is_invalid_params,
    "task.attach",
    json!({"task_id":"t6","file":{"path":"evidence.md","kind":"evidence"}}),
    "/file/kind",
    "kind",
    None
);
unknown_task_param_test!(
    unknown_verdict_is_invalid_params,
    "task.verdict",
    json!({"task_id":"t6","verdict":"accepted"}),
    "/verdict",
    "verdict",
    None
);
unknown_task_param_test!(
    unknown_list_state_is_invalid_params,
    "task.list",
    json!({"state":"open"}),
    "/state",
    "state",
    None
);
unknown_task_param_test!(
    unknown_list_kind_is_invalid_params,
    "task.list",
    json!({"kind":"work"}),
    "/kind",
    "kind",
    None
);

// Break caught: project filtering happens after pagination or ignores other task predicates and legacy fallback.
#[tokio::test]
async fn project_filter_intersects_worker_creator_parent_state_kind_and_open_pagination() {
    let rig = Rig::new().await;
    for project in ["alpha", "beta", "gamma"] {
        rig.org
            .execute(
                OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                    move_root: false,
                },
                now(),
            )
            .await
            .unwrap();
    }
    rig.org
        .execute(
            OrgCommand::Adopt {
                agent_id: "beta/worker".parse().unwrap(),
                pane: "%5".into(),
                session_id: None,
            },
            now(),
        )
        .await
        .unwrap();
    let mut root = created("Root");
    root.project_id = Some("alpha".parse().unwrap());
    rig.run(TaskCommand::Create(root)).await;
    let parent: atm_org::TaskId = format!("t{}", rig.org.status().unwrap()).parse().unwrap();
    let mut expected = Vec::new();
    for (project, cancelled) in [
        ("alpha", false),
        ("beta", false),
        ("alpha", true),
        ("alpha", false),
    ] {
        let mut body = created("Child");
        body.project_id = Some(project.parse().unwrap());
        body.parent = Some(parent.clone());
        rig.run(TaskCommand::Create(body)).await;
        let id: atm_org::TaskId = format!("t{}", rig.org.status().unwrap()).parse().unwrap();
        rig.run(TaskCommand::Assign {
            task_id: id.clone(),
            agent_id: "beta/worker".parse().unwrap(),
            by: actor("owner"),
        })
        .await;
        if cancelled {
            rig.run(TaskCommand::SetState {
                task_id: id.clone(),
                state: TaskState::Cancelled,
                reason: None,
                by: actor("owner"),
            })
            .await;
        }
        if project == "alpha" && !cancelled {
            expected.push(id.to_string());
        }
    }
    let filter = json!({"project_id":"alpha", "agent_id":"beta/worker", "creator":"owner", "parent":parent, "kind":"work", "state":"assigned", "open_only":true, "limit":1});
    let first = rig.ok(None, "task.list", filter.clone()).await;
    assert_eq!(first["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(first["tasks"][0]["task_id"], expected[0]);
    assert_eq!(first["next_after"], expected[0]);
    let mut second_filter = filter;
    second_filter["after"] = first["next_after"].clone();
    let second = rig.ok(None, "task.list", second_filter).await;
    assert_eq!(second["tasks"][0]["task_id"], expected[1]);
    assert!(second["next_after"].is_null());
    // The legacy owner-created task is unplaced, then follows its current worker, then becomes unplaced again.
    assert!(rig
        .ok(None, "task.list", json!({"project_id":"project"}))
        .await["tasks"]
        .as_array()
        .unwrap()
        .is_empty());
    rig.assign().await;
    assert_eq!(
        rig.ok(None, "task.list", json!({"project_id":"project"}))
            .await["tasks"][0]["task_id"],
        "t6"
    );
    rig.run(TaskCommand::SetState {
        task_id: "t6".parse().unwrap(),
        state: TaskState::Open,
        reason: None,
        by: actor("owner"),
    })
    .await;
    assert!(rig
        .ok(None, "task.list", json!({"project_id":"project"}))
        .await["tasks"]
        .as_array()
        .unwrap()
        .is_empty());
    let before = rig.snapshot().await;
    let seq = rig.org.status().unwrap();
    for (project, code) in [
        (json!("missing"), "not_found"),
        (json!("../alpha"), "invalid_params"),
        (json!(3), "invalid_params"),
    ] {
        assert_eq!(
            rig.call(None, "task.list", json!({"project_id":project}))
                .await
                .unwrap_err()
                .0
                .code,
            code
        );
    }
    assert_eq!(rig.snapshot().await, before);
    assert_eq!(rig.org.status().unwrap(), seq);
}

// Break caught: filtered pages skip overflow rows, exceed 512 KiB, or accept one individually oversized summary.
#[tokio::test]
async fn project_filter_preserves_byte_bounded_pagination_without_lost_rows() {
    let rig = Rig::new().await;
    for project in ["alpha", "beta", "gamma"] {
        rig.org
            .execute(
                OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                    move_root: false,
                },
                now(),
            )
            .await
            .unwrap();
    }
    let mut expected = Vec::new();
    for project in ["alpha", "beta", "alpha", "beta", "alpha"] {
        let mut body = created(&"x".repeat(180_000));
        body.project_id = Some(project.parse().unwrap());
        rig.run(TaskCommand::Create(body)).await;
        if project == "alpha" {
            expected.push(format!("t{}", rig.org.status().unwrap()));
        }
    }
    let mut found = Vec::new();
    let mut after = Value::Null;
    for size in [2, 1] {
        let page = rig
            .ok(
                None,
                "task.list",
                json!({"project_id":"alpha", "open_only":true, "limit":1000, "after":after}),
            )
            .await;
        assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
        assert_eq!(page["tasks"].as_array().unwrap().len(), size);
        found.extend(
            page["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["task_id"].as_str().unwrap().to_owned()),
        );
        after = page["next_after"].clone();
    }
    assert!(after.is_null());
    assert_eq!(found, expected);
    let mut body = created(&"x".repeat(600_000));
    body.project_id = Some("gamma".parse().unwrap());
    rig.run(TaskCommand::Create(body)).await;
    assert_eq!(
        rig.call(None, "task.list", json!({"project_id":"gamma"}))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
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
