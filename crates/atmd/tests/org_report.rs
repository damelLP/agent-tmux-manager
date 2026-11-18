//! Synthetic reports through the real private server/actor, never a harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{OrgCommand, TaskActor, TaskCommand, TaskId, TaskSubmission, TaskVerdict};
use atm_protocol::{ClientMessage, DaemonMessage, ResponseError, ResponsePayload};
use atmd::{
    org::{agents::AgentHandle, decisions::DecisionHandle, messages::MessageHandle, OrgHandle},
    registry::spawn_registry,
    server::{methods::MethodServices, DaemonServer},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;

fn at() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}
fn actor(who: &str) -> TaskActor {
    TaskActor {
        who: who.parse().unwrap(),
        manager_of: None,
    }
}
const SEATS: [(&str, &str, &str); 6] = [
    ("alpha/worker", "%1", "lead"),
    ("alpha/lead", "%2", "gamma/chief"),
    ("beta/worker", "%3", "lead"),
    ("beta/lead", "%4", "owner"),
    ("gamma/chief", "%5", "owner"),
    ("gamma/other", "%6", "owner"),
];
fn role(manager: &str) -> String {
    format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nPRIVATE_ROLE\n")
}
fn ids(report: &Value) -> Vec<String> {
    report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].as_str().unwrap().into())
        .collect()
}
struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    cancel: CancellationToken,
    server: tokio::task::JoinHandle<Result<(), atmd::server::ServerError>>,
    control: tokio::task::JoinHandle<()>,
    org_cancel: CancellationToken,
}
impl Rig {
    async fn new(count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let org_cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), org_cancel.clone()).await;
        for name in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .take(count)
        {
            let root = dir.path().join(name);
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: name.parse().unwrap(),
                    root: root.to_str().unwrap().into(),
                    move_root: false,
                },
                at(),
            )
            .await
            .unwrap();
        }
        for (id, pane, manager) in SEATS {
            org.execute(
                OrgCommand::Adopt {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                },
                at(),
            )
            .await
            .unwrap();
            let (project, name) = id.split_once('/').unwrap();
            fs::write(
                dir.path()
                    .join(project)
                    .join(format!(".atm/roles/{name}.md")),
                role(manager),
            )
            .unwrap();
        }
        assert!(!dir.path().join("gamma/.git").exists());
        let cancel = CancellationToken::new();
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        services.agents = Some(AgentHandle::disabled("report fixture"));
        services.messages = Some(MessageHandle::disabled("report fixture"));
        services.decisions = Some(DecisionHandle::disabled("report fixture"));
        let (control, control_task) = atmd::control::service::spawn_control(
            Box::new(atm_tmux::MockTmuxClient::new()),
            services.registry.clone(),
            org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        let server =
            DaemonServer::with_services(dir.path().join("atm.sock"), services, cancel.clone());
        let mut run = Box::pin(async move { server.run().await });
        std::future::poll_fn(|cx| {
            use std::{future::Future, task::Poll};
            assert!(run.as_mut().poll(cx).is_pending()); // bind precedes the first accept
            Poll::Ready(())
        })
        .await;
        Self {
            dir,
            org,
            cancel,
            server: tokio::spawn(run),
            control: control_task,
            org_cancel,
        }
    }
    fn path(&self, id: &str) -> PathBuf {
        let (project, name) = id.split_once('/').unwrap();
        self.dir
            .path()
            .join(project)
            .join(format!(".atm/roles/{name}.md"))
    }
    async fn rpc(
        &self,
        pane: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, ResponseError> {
        tokio::time::timeout(Duration::from_secs(60), async {
            let mut socket = BufReader::new(
                UnixStream::connect(self.dir.path().join("atm.sock"))
                    .await
                    .unwrap(),
            );
            for msg in [
                ClientMessage::connect(Some("report-test".into())),
                ClientMessage::request(
                    "report".into(),
                    method.into(),
                    params,
                    pane.map(str::to_owned),
                ),
            ] {
                socket
                    .get_mut()
                    .write_all(format!("{}\n", serde_json::to_string(&msg).unwrap()).as_bytes())
                    .await
                    .unwrap();
                let mut line = String::new();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                match serde_json::from_str::<DaemonMessage>(&line).unwrap() {
                    DaemonMessage::Connected { methods, .. } => {
                        assert!(
                            methods.iter().any(|m| m == "org.report"),
                            "org.report capability missing"
                        );
                    }
                    DaemonMessage::Response {
                        payload: ResponsePayload::Result(v),
                        ..
                    } => return Ok(v),
                    DaemonMessage::Response {
                        payload: ResponsePayload::Error(e),
                        ..
                    } => return Err(e),
                    other => panic!("unexpected {other:?}"),
                }
            }
            panic!("missing report response")
        })
        .await
        .unwrap()
    }
    async fn report(&self, pane: Option<&str>, params: Value) -> Value {
        self.rpc(pane, "org.report", params).await.unwrap()
    }
    async fn execute(&self, command: TaskCommand) -> Vec<atmd::eventlog::Envelope> {
        self.org
            .execute(OrgCommand::Task(command), at())
            .await
            .unwrap()
    }
    async fn task(&self, title: &str, creator: &str, assignee: Option<&str>) -> TaskId {
        let result = self.execute(TaskCommand::Create(serde_json::from_value(json!({"title":title,"kind":"work","creator":creator,"measure":{"kind":"owner"},"files":[]})).unwrap())).await;
        let id = format!("t{}", result[0].seq).parse().unwrap();
        if let Some(agent) = assignee {
            self.assign(&id, agent).await;
        }
        id
    }
    async fn assign(&self, id: &TaskId, agent: &str) {
        self.execute(TaskCommand::Assign {
            task_id: id.clone(),
            agent_id: agent.parse().unwrap(),
            by: actor("owner"),
        })
        .await;
    }
    async fn request(&self, who: &str, choice_by: &str) {
        let result = self.org.execute(OrgCommand::Decision(atm_org::DecisionCommand::Request(serde_json::from_value(json!({"requester":who,"kind":"plan","title":"PRIVATE_DECISION","context":"PRIVATE_CONTEXT","files":["PRIVATE_PATH"],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true})).unwrap())), at()).await.unwrap();
        self.org
            .execute(
                OrgCommand::Decision(atm_org::DecisionCommand::TakeProvisionally(
                    atm_org::ProvisionalChoice {
                        relayed_by: None,
                        decision_id: atm_org::DecisionId::new(result[0].seq),
                        option: "yes".into(),
                        by: choice_by.parse().unwrap(),
                    },
                )),
                at(),
            )
            .await
            .unwrap();
    }
    async fn stop(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(60), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.control.await.unwrap();
        self.org_cancel.cancel();
    }
}

// Break caught: server reports follow a worker project or selected project expands visibility.
#[tokio::test]
async fn recorded_project_scopes_reports_without_changing_visibility_or_decision_ownership() {
    let rig = Rig::new(3).await;
    let made = rig
        .rpc(
            None,
            "task.create",
            json!({"title":"Targeted", "project_id":"alpha", "measure":{"kind":"owner"}}),
        )
        .await
        .unwrap();
    let id: TaskId = serde_json::from_value(made["task_id"].clone()).unwrap();
    rig.assign(&id, "beta/worker").await;
    rig.request("beta/worker", "beta/worker").await;
    let got = rig.report(None, json!({})).await;
    assert_eq!(got["tasks"][0]["project_id"], "alpha");
    assert_eq!(got["tasks"][0]["agent_id"], "beta/worker");
    assert_eq!(got["projects"][0]["tasks"]["open"], 1);
    assert_eq!(got["projects"][1]["decisions"]["agent_decided"], 1);
    assert_eq!(
        ids(&rig.report(Some("%3"), json!({"project_id":"alpha"})).await),
        [id.to_string()]
    );
    assert!(ids(&rig.report(Some("%1"), json!({"project_id":"alpha"})).await).is_empty());
    assert!(ids(&rig.report(None, json!({"project_id":"beta"})).await).is_empty());
    rig.execute(TaskCommand::SetState {
        task_id: id.clone(),
        state: atm_org::TaskState::Open,
        reason: None,
        by: actor("owner"),
    })
    .await;
    let reopened = rig.report(None, json!({"project_id":"alpha"})).await;
    assert_eq!(reopened["tasks"][0]["project_id"], "alpha");
    assert_eq!(reopened["tasks"][0]["agent_id"], "beta/worker");
    rig.stop().await;
}

// Break caught: no capability, requester-only ACL, lexical/cwd/project-local
// ancestry, lost creator/last-holder rows, or aggregates derived from a page.
#[tokio::test]
async fn scoped_reports_use_current_transitive_roles_and_whole_scope_aggregates() {
    for count in [3, 5] {
        let rig = Rig::new(count).await;
        let own = rig.task("Own", "alpha/lead", None).await;
        let direct = rig.task("Direct", "owner", Some("alpha/worker")).await;
        let last = rig.task("Last held", "owner", Some("alpha/worker")).await;
        rig.execute(TaskCommand::SetState {
            task_id: last.clone(),
            state: atm_org::TaskState::Open,
            reason: None,
            by: actor("owner"),
        })
        .await;
        let created = rig
            .task("Created", "alpha/worker", Some("beta/worker"))
            .await;
        let hidden = rig
            .task("PRIVATE_HIDDEN_TASK", "owner", Some("beta/worker"))
            .await;
        let unplaced = rig.task("Unplaced", "owner", None).await;
        let chief = rig.task("Chief", "gamma/chief", None).await;
        rig.request("alpha/worker", "system").await;
        rig.request("beta/worker", "beta/worker").await;
        let seq = rig.org.status().unwrap();
        let all = rig.report(None, json!({})).await;
        assert_eq!(
            ids(&all),
            [
                own.clone(),
                direct.clone(),
                last.clone(),
                created.clone(),
                hidden.clone(),
                unplaced,
                chief.clone()
            ]
            .map(|id| id.to_string())
        );
        assert_eq!(all["seq"], seq);
        assert_eq!(all["scope"]["sees"], "all");
        let unbound = rig.report(Some("%99"), json!({})).await;
        assert_eq!(ids(&unbound), ids(&all));
        let lead = rig.report(Some("%2"), json!({})).await;
        assert_eq!(
            ids(&lead),
            [own.clone(), direct.clone(), last.clone(), created.clone()].map(|id| id.to_string())
        );
        assert_eq!(lead["scope"]["reader"], "alpha/lead");
        assert_eq!(lead["scope"]["sees"], "reports");
        assert_eq!(lead["projects"][0]["tasks"]["open"], 3);
        assert_eq!(lead["projects"][0]["decisions"]["taken_for_owner"], 1);
        assert_eq!(lead["projects"][1]["decisions"]["agent_decided"], 0);
        assert!(!lead.to_string().contains("PRIVATE_"));
        let transitive = rig.report(Some("%5"), json!({})).await;
        assert_eq!(
            ids(&transitive),
            [
                own.clone(),
                direct.clone(),
                last.clone(),
                created.clone(),
                chief.clone()
            ]
            .map(|id| id.to_string())
        );
        let page = rig.report(Some("%5"), json!({"limit":1})).await;
        assert_eq!(ids(&page), [own.to_string()]);
        assert_eq!(page["next_after"], own.to_string());
        assert_eq!(page["projects"], transitive["projects"]);
        let filtered = rig.report(Some("%2"), json!({"project_id":"beta"})).await;
        assert_eq!(ids(&filtered), [created.to_string()]);
        assert_eq!(filtered["projects"][0]["decisions"]["agent_decided"], 0);
        fs::write(rig.path("alpha/lead"), role("beta/lead")).unwrap();
        assert_eq!(
            ids(&rig.report(Some("%5"), json!({})).await),
            [chief.to_string()]
        );
        assert_eq!(
            ids(&rig.report(Some("%4"), json!({})).await),
            [own, direct, last, created, hidden].map(|id| id.to_string())
        );
        assert_eq!(
            rig.org.status().unwrap(),
            seq,
            "reads/role edits must not append facts"
        );
        assert_eq!(
            rig.rpc(Some("bad"), "org.report", json!({}))
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
        let info = rig.rpc(None, "daemon.info", json!({})).await.unwrap();
        assert!(info["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("org.report")));
        rig.stop().await;
    }
}

// Break caught: configuration faults fail all named readers or grant a partial
// chain; owner/self visibility must not depend on that broken chain.
#[tokio::test]
async fn broken_configuration_omits_descendants_without_failing_owner_or_self() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(3).await;
    let own = rig.task("Own", "alpha/lead", None).await;
    let report = rig
        .task("PRIVATE_HIDDEN_TASK", "owner", Some("alpha/worker"))
        .await;
    let path = rig.path("alpha/worker");
    for manager in ["missing", "alpha/worker", "system"] {
        fs::write(&path, role(manager)).unwrap();
        assert_eq!(
            ids(&rig.report(Some("%2"), json!({})).await),
            [own.to_string()]
        );
        assert_eq!(
            ids(&rig.report(Some("%1"), json!({})).await),
            [report.to_string()]
        );
        assert_eq!(ids(&rig.report(None, json!({})).await).len(), 2);
    }
    fs::write(&path, "+++\nPRIVATE_INVALID_ROLE\n+++\n").unwrap();
    assert_eq!(
        ids(&rig.report(Some("%2"), json!({})).await),
        [own.to_string()]
    );
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        ids(&rig.report(Some("%2"), json!({})).await),
        [own.to_string()]
    );
    assert_eq!(ids(&rig.report(None, json!({})).await).len(), 2);
    fs::remove_dir(&path).unwrap();
    fs::write(&path, role("lead")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&path).is_ok() {
        eprintln!("skipping unreadable role case: chmod 000 does not prevent this user from opening the file");
    } else {
        assert_eq!(
            fs::read_to_string(&path).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            ids(&rig.report(Some("%2"), json!({})).await),
            [own.to_string()]
        );
        assert_eq!(ids(&rig.report(None, json!({})).await).len(), 2);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        ids(&rig.report(Some("%2"), json!({})).await),
        [own.to_string(), report.to_string()]
    );
    fs::write(rig.path("alpha/lead"), role("missing")).unwrap();
    let chief = rig.report(Some("%5"), json!({})).await;
    assert!(
        ids(&chief).is_empty(),
        "a valid prefix must never grant ancestry"
    );
    assert!(!chief.to_string().contains("PRIVATE_"));
    rig.stop().await;
}

// Break caught: actor-inserted cost records make automatic review closure count
// as a second owner verdict, or reads append accounting/notification events.
#[tokio::test]
async fn actual_parent_cost_review_close_sequence_reports_one_owner_verdict() {
    let rig = Rig::new(3).await;
    let parent = rig.task("Parent", "owner", Some("alpha/worker")).await;
    rig.execute(TaskCommand::Submit {
        task_id: parent.clone(),
        what: TaskSubmission::Result,
        files: vec![],
        by: actor("alpha/worker"),
    })
    .await;
    let created = serde_json::from_value(json!({"title":"Review","kind":"review","creator":"owner","judges":parent,"measure":{"kind":"owner"},"files":[]})).unwrap();
    let made = rig.execute(TaskCommand::Create(created)).await;
    let review: TaskId = format!("t{}", made[0].seq).parse().unwrap();
    rig.assign(&review, "gamma/other").await;
    rig.execute(TaskCommand::Submit {
        task_id: review.clone(),
        what: TaskSubmission::Result,
        files: vec![],
        by: actor("gamma/other"),
    })
    .await;
    rig.execute(TaskCommand::StartReview {
        task_id: parent.clone(),
        review_task: review.clone(),
        by: actor("owner"),
    })
    .await;
    let closed = rig
        .execute(TaskCommand::Verdict {
            task_id: parent.clone(),
            verdict: TaskVerdict::Accepted,
            review_task: Some(review.clone()),
            note: Some("PRIVATE_VERDICT".into()),
            by: actor("owner"),
        })
        .await;
    assert_eq!(
        closed.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        [
            "task.verdict",
            "task.cost_recorded",
            "task.verdict",
            "task.cost_recorded"
        ]
    );
    let seq = rig.org.status().unwrap();
    let value = rig.report(None, json!({})).await;
    assert_eq!(value["seq"], seq);
    assert_eq!(value["tasks"][0]["owner_step_ins"]["verdict"], 1);
    assert_eq!(value["tasks"][1]["owner_step_ins"]["verdict"], 0);
    let state = rig.org.snapshot().await.unwrap();
    let now: DateTime<Utc> = value["now"].as_str().unwrap().parse().unwrap();
    for row in value["tasks"].as_array().unwrap() {
        let usage = serde_json::to_value(
            state
                .task_cost(&row["task_id"].as_str().unwrap().parse().unwrap(), now)
                .unwrap(),
        )
        .unwrap();
        for (key, expected) in usage.as_object().unwrap() {
            assert_eq!(&row[key], expected);
        }
    }
    assert_eq!(rig.org.status().unwrap(), seq);
    assert!(!value.to_string().contains("PRIVATE_"));
    rig.stop().await;
}
