//! Synthetic reporting-line contracts: configured neutral projects, real files and writer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{AgentId, OrgCommand, OrgState, TaskId};
use atmd::{
    org::OrgHandle,
    registry::spawn_registry,
    server::methods::{
        router, CallContext, CallerId, CallerPane, MethodError, MethodRouter, MethodServices,
    },
};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};
use tokio_util::sync::CancellationToken;

const SEATS: [(&str, &str, &str); 8] = [
    ("alpha/worker", "%1", "lead"),
    ("alpha/lead", "%2", "gamma/chief"),
    ("alpha/reviewer", "%3", "lead"),
    ("alpha/other", "%4", "owner"),
    ("beta/lead", "%5", "owner"),
    ("beta/worker", "%6", "lead"),
    ("beta/manager", "%7", "gamma/chief"),
    ("gamma/chief", "%8", "owner"),
];

fn caller(id: &str) -> CallContext {
    CallContext {
        client_id: CallerId("authority-test".into()),
        from_pane: SEATS
            .iter()
            .find(|(seat, _, _)| *seat == id)
            .map(|(_, pane, _)| CallerPane((*pane).into())),
    }
}

struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    router: MethodRouter,
    cancel: CancellationToken,
}
impl Rig {
    async fn new(count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for project in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .take(count)
        {
            let root = dir.path().join(project);
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            fs::write(
                root.join(".atm/project.toml"),
                format!("id = '{project}'\n"),
            )
            .unwrap();
            let config = atm_org::parse_project(
                &fs::read_to_string(root.join(".atm/project.toml")).unwrap(),
            )
            .unwrap();
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: config.id,
                    root: root.to_str().unwrap().into(),
                    move_root: false,
                },
                chrono::Utc::now(),
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
                chrono::Utc::now(),
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
        // Gamma is a non-repository project folder, not a department.
        assert!(!dir.path().join("gamma/.git").exists());
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        Self {
            dir,
            org,
            router: router(services).unwrap(),
            cancel,
        }
    }
    fn path(&self, id: &str) -> PathBuf {
        let (project, name) = id.split_once('/').unwrap();
        self.dir
            .path()
            .join(project)
            .join(format!(".atm/roles/{name}.md"))
    }
    fn manager(&self, id: &str, manager: &str) {
        fs::write(self.path(id), role(manager)).unwrap();
    }
    async fn call(&self, who: &str, method: &str, params: Value) -> Result<Value, MethodError> {
        self.router.call(caller(who), method, params).await
    }
    async fn ok(&self, who: &str, method: &str, params: Value) -> Value {
        self.call(who, method, params).await.unwrap()
    }
    async fn state(&self) -> OrgState {
        self.org.snapshot().await.unwrap()
    }
    async fn create(&self, who: &str, measure: Value, parent: Option<&TaskId>) -> TaskId {
        serde_json::from_value(
            self.ok(
                who,
                "task.create",
                json!({
                    "title":"Work", "project_id":"alpha", "measure":measure, "parent":parent,
                }),
            )
            .await["task_id"]
                .clone(),
        )
        .unwrap()
    }
    async fn assigned(&self, who: &str, assignee: &str, measure: Value) -> TaskId {
        let id = self.create(who, measure, None).await;
        self.ok(
            who,
            "task.assign",
            json!({"task_id":id,"agent_id":assignee}),
        )
        .await;
        id
    }
    async fn prepared(&self, op: &str) -> (String, Value) {
        let id = self.assigned("owner", "alpha/worker", mechanical()).await;
        let params = match op {
            "assign" => json!({"task_id":id,"agent_id":"alpha/other"}),
            "open" | "cancelled" => json!({"task_id":id,"state":op}),
            "review" | "accepted" | "changes_requested" => {
                self.ok("alpha/worker", "task.submit", json!({"task_id":id}))
                    .await;
                if op == "review" {
                    let review = self.ok("owner", "task.create", json!({
                        "project_id":"alpha", "title":"Review", "kind":"review", "judges":id,
                        "measure":{"kind":"owner"},
                    })).await;
                    json!({"task_id":id,"review_task":review["task_id"]})
                } else {
                    json!({"task_id":id,"verdict":op})
                }
            }
            _ => panic!("unknown operation"),
        };
        let method = match op {
            "assign" => "task.assign",
            "open" | "cancelled" => "task.set_state",
            "review" => "task.review",
            _ => "task.verdict",
        };
        (method.into(), params)
    }
    async fn refused(&self, who: &str, method: &str, params: Value) -> String {
        let before = self.state().await;
        let cursor = self.org.status().unwrap();
        let error = self.call(who, method, params).await.unwrap_err();
        assert_eq!(error.0.code, "refused", "{}", error.0.message);
        assert_eq!(self.state().await, before, "refusal mutated state");
        assert_eq!(
            self.org.status().unwrap(),
            cursor,
            "refusal appended an event"
        );
        error.0.message
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn role(manager: &str) -> String {
    format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nSynthetic role\n")
}
fn mechanical() -> Value {
    json!({"kind":"mechanical","command":"check"})
}
const MOVES: [&str; 6] = [
    "assign",
    "open",
    "cancelled",
    "review",
    "accepted",
    "changes_requested",
];

// Break caught: selecting a project grants creation without a validated ancestry witness.
#[tokio::test]
async fn target_creation_requires_one_registered_witness_and_counts_down_seats() {
    let rig = Rig::new(5).await;
    let request = json!({"title":"Targeted", "project_id":"alpha", "measure":mechanical()});
    assert_eq!(
        rig.ok("gamma/chief", "task.create", request.clone()).await["changed"],
        true
    );
    rig.refused("beta/lead", "task.create", request.clone())
        .await;
    rig.refused(
        "alpha/worker",
        "task.create",
        json!({"title":"No witness", "project_id":"delta", "measure":mechanical()}),
    )
    .await;
    rig.ok(
        "owner",
        "task.create",
        json!({"title":"No witness needed", "project_id":"delta", "measure":mechanical()}),
    )
    .await;
    // A differently named manager works; department membership is not an authority input.
    rig.manager("alpha/lead", "beta/manager");
    rig.ok("beta/manager", "task.create", request).await;
    rig.org
        .execute(
            OrgCommand::Down {
                agent_id: "beta/manager".parse().unwrap(),
                reason: "Fixture".into(),
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    // An invalid unrelated chain must not hide a later, valid down-seat witness.
    fs::write(rig.path("beta/lead"), "invalid unrelated role").unwrap();
    rig.ok(
        "gamma/chief",
        "task.create",
        json!({"title":"Down witness", "project_id":"beta", "measure":mechanical()}),
    )
    .await;
}

// Break caught: create accepts a partial/cached chain or gives creator/owner new role-file dependencies.
#[tokio::test]
async fn target_creation_rechecks_the_full_chain_and_bypasses_roles_for_own_and_owner() {
    let rig = Rig::new(3).await;
    let request = json!({"title":"Targeted", "project_id":"alpha", "measure":mechanical()});
    rig.ok("gamma/chief", "task.create", request.clone()).await;
    for manager in ["alpha/lead", "beta/unknown", "system", "../bad"] {
        rig.manager("gamma/chief", manager);
        rig.refused("gamma/chief", "task.create", request.clone())
            .await;
        rig.ok("owner", "task.create", request.clone()).await;
        rig.ok("alpha/worker", "task.create", request.clone()).await;
    }
    rig.manager("gamma/chief", "owner");
    rig.ok("gamma/chief", "task.create", request.clone()).await;
    // Remove every path to chief in the target project: no saved manager grants.
    rig.manager("alpha/lead", "owner");
    rig.refused("gamma/chief", "task.create", request).await;
}

// Break caught: an unrelated project lead still gets global authority from its name.
#[tokio::test]
async fn unrelated_project_lead_is_refused_for_every_authority_move() {
    for count in [3, 5] {
        for op in MOVES {
            let rig = Rig::new(count).await;
            let (method, params) = rig.prepared(op).await;
            rig.refused("beta/lead", &method, params.clone()).await;
            assert_eq!(rig.ok("owner", &method, params).await["changed"], true);
        }
    }
}

// Break caught: privilege checks still require a special name or one/two repositories.
#[tokio::test]
async fn project_count_and_non_repo_roots_do_not_change_authority() {
    for count in [3, 5] {
        for who in ["alpha/lead", "gamma/chief", "beta/manager"] {
            for op in MOVES {
                let rig = Rig::new(count).await;
                rig.manager("alpha/lead", "beta/manager");
                let (method, params) = rig.prepared(op).await;
                assert_eq!(rig.ok(who, &method, params).await["changed"], true);
            }
        }
    }
}

// Break caught: an assignee delegates the chief's task through authority it does not have.
#[tokio::test]
async fn delegation_creates_a_subtask_not_reassignment_of_the_managers_task() {
    let rig = Rig::new(3).await;
    // The creator is gamma/chief; its own intent applies, with no file restriction.
    let parent = rig
        .ok(
            "gamma/chief",
            "task.create",
            json!({"title":"Goal","measure":mechanical()}),
        )
        .await;
    let parent: TaskId = serde_json::from_value(parent["task_id"].clone()).unwrap();
    rig.ok(
        "gamma/chief",
        "task.assign",
        json!({"task_id":parent,"agent_id":"alpha/lead"}),
    )
    .await;
    rig.refused(
        "alpha/lead",
        "task.assign",
        json!({"task_id":parent,"agent_id":"alpha/worker"}),
    )
    .await;
    let child = rig.create("alpha/lead", mechanical(), Some(&parent)).await;
    assert_eq!(
        rig.ok(
            "alpha/lead",
            "task.assign",
            json!({"task_id":child,"agent_id":"alpha/worker"})
        )
        .await["changed"],
        true
    );
    let state = rig.state().await;
    assert_eq!(
        state.task(&parent).unwrap().agent_id,
        Some("alpha/lead".parse().unwrap())
    );
    assert_eq!(state.task(&child).unwrap().parent, Some(parent));
}

// Break caught: manager of the proposed recipient controls an as-yet unassigned task.
#[tokio::test]
async fn unassigned_tasks_need_creator_or_owner_and_creators_cross_projects_keep_authority() {
    let rig = Rig::new(3).await;
    let id = rig.create("owner", mechanical(), None).await;
    for who in ["alpha/lead", "gamma/chief", "beta/lead"] {
        rig.refused(
            who,
            "task.assign",
            json!({"task_id":id,"agent_id":"alpha/worker"}),
        )
        .await;
    }
    let made = rig
        .ok(
            "beta/lead",
            "task.create",
            json!({"title":"Cross-project goal","measure":mechanical()}),
        )
        .await;
    let id = &made["task_id"];
    rig.ok(
        "beta/lead",
        "task.assign",
        json!({"task_id":id,"agent_id":"alpha/worker"}),
    )
    .await;
    rig.manager("alpha/worker", "invalid/unknown");
    rig.ok(
        "beta/lead",
        "task.set_state",
        json!({"task_id":id,"state":"cancelled"}),
    )
    .await;
}

// Break caught: stale stored roles or missing live files restore former manager authority.
#[tokio::test]
async fn adopted_and_started_seats_use_current_role_files() {
    for started in [false, true] {
        let rig = Rig::new(3).await;
        if started {
            let agent_id: AgentId = "alpha/worker".parse().unwrap();
            rig.org
                .execute(
                    OrgCommand::Down {
                        agent_id: agent_id.clone(),
                        reason: "Fixture restart".into(),
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            rig.org
                .execute(
                    OrgCommand::Up {
                        agent_id: agent_id.clone(),
                        role: atm_org::parse_role("worker", &role("beta/lead")).unwrap(),
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            rig.org
                .execute(
                    OrgCommand::PaneCreated {
                        agent_id,
                        pane: "%1".into(),
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
        } else {
            assert!(rig
                .state()
                .await
                .agent(&"alpha/worker".parse().unwrap())
                .unwrap()
                .role
                .is_none());
        }
        let id = rig.assigned("owner", "alpha/worker", mechanical()).await;
        rig.refused(
            "beta/lead",
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
        rig.ok(
            "alpha/lead",
            "task.assign",
            json!({"task_id":id,"agent_id":"alpha/worker"}),
        )
        .await;
        rig.manager("alpha/worker", "beta/lead");
        rig.refused(
            "alpha/lead",
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
        rig.ok(
            "beta/lead",
            "task.assign",
            json!({"task_id":id,"agent_id":"alpha/worker"}),
        )
        .await;
        fs::remove_file(rig.path("alpha/worker")).unwrap();
        rig.refused(
            "beta/lead",
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
        rig.ok(
            "owner",
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
    }
}

// Break caught: a missing manager role fabricates higher ancestors or falls back to saved config.
#[tokio::test]
async fn missing_roles_end_the_chain_without_cached_fallback() {
    let rig = Rig::new(3).await;
    let id = rig.assigned("owner", "alpha/worker", mechanical()).await;
    fs::remove_file(rig.path("alpha/lead")).unwrap();
    rig.ok(
        "alpha/lead",
        "task.assign",
        json!({"task_id":id,"agent_id":"alpha/worker"}),
    )
    .await;
    rig.refused(
        "gamma/chief",
        "task.set_state",
        json!({"task_id":id,"state":"cancelled"}),
    )
    .await;
    fs::remove_file(rig.path("alpha/worker")).unwrap();
    rig.refused(
        "alpha/lead",
        "task.set_state",
        json!({"task_id":id,"state":"cancelled"}),
    )
    .await;
}

// Break caught: a partial chain grants before a malformed later node/cycle is checked.
#[tokio::test]
async fn broken_chains_never_grant_partial_authority() {
    for (node, contents) in [
        ("alpha/worker", role("alpha/worker")),
        ("alpha/lead", role("alpha/worker")),
        ("gamma/chief", role("alpha/lead")),
        ("gamma/chief", role("beta/unknown")),
        ("gamma/chief", role("system")),
        ("gamma/chief", "secret invalid role".into()),
        ("gamma/chief", role("../bad")),
    ] {
        let rig = Rig::new(3).await;
        let id = rig.assigned("owner", "alpha/worker", mechanical()).await;
        fs::write(rig.path(node), contents).unwrap();
        let message = rig
            .refused(
                "alpha/lead",
                "task.set_state",
                json!({"task_id":id,"state":"cancelled"}),
            )
            .await;
        assert!(message.contains(node), "broken node {node}: {message}");
        assert!(!message.contains("secret"), "role body leaked");
        rig.ok(
            "owner",
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
    }
}

// Break caught: role I/O is coupled to unrelated nodes/status or to own-agent operations.
#[tokio::test]
async fn unrelated_broken_roles_and_down_intermediates_do_not_change_the_line() {
    let rig = Rig::new(5).await;
    let id = rig.assigned("owner", "alpha/worker", mechanical()).await;
    fs::write(rig.path("beta/lead"), "invalid unrelated role").unwrap();
    rig.org
        .execute(
            OrgCommand::Down {
                agent_id: "alpha/lead".parse().unwrap(),
                reason: "Fixture".into(),
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    rig.ok(
        "gamma/chief",
        "task.assign",
        json!({"task_id":id,"agent_id":"alpha/worker"}),
    )
    .await;
    fs::write(rig.path("alpha/worker"), "invalid own role").unwrap();
    for state in ["working", "waiting", "blocked"] {
        rig.ok(
            "alpha/worker",
            "task.set_state",
            json!({"task_id":id,"state":state,"reason":"Fixture"}),
        )
        .await;
    }
    rig.ok(
        "alpha/worker",
        "task.attach",
        json!({"task_id":id,"file":{"path":"proof.md","kind":"evidence"}}),
    )
    .await;
    rig.ok("alpha/worker", "task.submit", json!({"task_id":id}))
        .await;
}

// Break caught: filesystem failures look like absent policy or leak role text.
#[tokio::test]
async fn invalid_role_files_fail_closed_at_the_broken_node() {
    for case in [
        "utf8",
        "oversized",
        "directory",
        "symlink",
        "missing-root",
        "unreadable",
    ] {
        let rig = Rig::new(3).await;
        let id = rig.assigned("owner", "alpha/worker", mechanical()).await;
        let path = rig.path("gamma/chief");
        match case {
            "utf8" => fs::write(&path, [0xff]).unwrap(),
            "oversized" => fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap(),
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "symlink" => {
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(rig.path("alpha/worker"), &path).unwrap();
            }
            "missing-root" => fs::rename(
                rig.dir.path().join("gamma"),
                rig.dir.path().join("gamma-moved"),
            )
            .unwrap(),
            "unreadable" => {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                if fs::File::open(&path).is_ok() {
                    eprintln!(
                        "skipping unreadable role case: chmod 000 does not prevent this user from opening the file"
                    );
                    continue;
                }
            }
            _ => unreachable!(),
        }
        let message = rig
            .refused(
                "alpha/lead",
                "task.set_state",
                json!({"task_id":id,"state":"cancelled"}),
            )
            .await;
        assert!(message.contains("gamma/chief"), "{case}: {message}");
        assert!(!message.contains("Synthetic role"));
        rig.ok(
            "owner",
            "task.set_state",
            json!({"task_id":id,"state":"cancelled"}),
        )
        .await;
    }
}

// Break caught: manager admission works in-process but not over the actual RPC boundary.
#[tokio::test]
async fn private_socket_enforces_reporting_lines_for_three_and_five_projects() {
    use atmd::server::DaemonServer;
    use std::{future::Future, task::Poll, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
    };
    let check = async {
        for count in [3, 5] {
            let rig = Rig::new(count).await;
            let id = rig.assigned("owner", "alpha/worker", mechanical()).await;
            let mut services = MethodServices::new(spawn_registry());
            services.org = rig.org.clone();
            services.agents = Some(atmd::org::agents::AgentHandle::disabled(
                "private authority fixture",
            ));
            services.messages = Some(atmd::org::messages::MessageHandle::disabled(
                "private authority fixture",
            ));
            let (control, control_task) = atmd::control::service::spawn_control(
                Box::new(atm_tmux::MockTmuxClient::new()),
                services.registry.clone(),
                rig.org.clone(),
                rig.cancel.clone(),
            );
            services.control = Some(control);
            let socket = rig.dir.path().join("authority.sock");
            let server = DaemonServer::with_services(&socket, services, rig.cancel.clone());
            let mut run = Box::pin(async move { server.run().await });
            std::future::poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let server_task = tokio::spawn(run);
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
            assert_eq!(hello["type"], "connected");
            let before = rig.state().await;
            for (pane, expected) in [("%5", false), ("%8", true)] {
                let request = json!({"type":"request","protocol_version":{"major":1,"minor":1},"id":pane,"from_pane":pane,"method":"task.set_state","params":{"task_id":id,"state":"cancelled"}});
                write
                    .write_all(format!("{request}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                read.read_line(&mut line).await.unwrap();
                let response: Value = serde_json::from_str(&line).unwrap();
                if expected {
                    assert_eq!(response["result"]["changed"], true, "{response}");
                } else {
                    assert_eq!(response["error"]["code"], "refused", "{response}");
                    assert_eq!(rig.state().await, before);
                }
            }
            assert_eq!(
                rig.state().await.task(&id).unwrap().state,
                atm_org::TaskState::Cancelled
            );
            rig.cancel.cancel();
            server_task.await.unwrap().unwrap();
            control_task.await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(60), check)
        .await
        .unwrap();
}

// Break caught: being a manager bypasses own-agent and result-measure restrictions.
#[tokio::test]
async fn managers_cannot_work_for_agents_or_bypass_measures() {
    for measure in [
        json!({"kind":"owner"}),
        json!({"kind":"reviewed","rubric":"Independent proof"}),
    ] {
        let rig = Rig::new(3).await;
        let id = rig.assigned("owner", "alpha/worker", measure).await;
        rig.refused(
            "gamma/chief",
            "task.set_state",
            json!({"task_id":id,"state":"working"}),
        )
        .await;
        rig.refused("gamma/chief", "task.submit", json!({"task_id":id}))
            .await;
        rig.ok("alpha/worker", "task.submit", json!({"task_id":id}))
            .await;
        rig.refused(
            "gamma/chief",
            "task.verdict",
            json!({"task_id":id,"verdict":"accepted"}),
        )
        .await;
        rig.ok(
            "owner",
            "task.verdict",
            json!({"task_id":id,"verdict":"accepted"}),
        )
        .await;
    }
}
